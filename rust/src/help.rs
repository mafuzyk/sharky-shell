//! Static help registry.
//!
//! One source of text for `sharky --help`, `sharky help [TOPIC]` and the shell
//! `help` builtin. It parses nothing, reads no configuration and touches no
//! files: the CLI dispatches `help` before a shell exists, and the builtin only
//! prints what this module holds.
//!
//! Topics are looked up in this order: builtin names, command-line flags and
//! subcommands, then the configuration guide (`sharky config explain`). That
//! last step keeps the configuration language prose in one place instead of
//! copying it here.

use std::io::Write;

use sharky_config::guide;

/// Shell builtin names, mirroring `shell::is_builtin_name`.
///
/// The `help` integration test parses this list and the predicate and fails
/// when they drift, so a new builtin without a topic cannot go unnoticed.
pub const BUILTINS: &[&str] = &[
    ":",
    "true",
    "false",
    "cd",
    "pushd",
    "popd",
    "dirs",
    "ulimit",
    "hash",
    "let",
    "times",
    "disown",
    "exit",
    "export",
    "unset",
    "set",
    "source",
    ".",
    "break",
    "continue",
    "return",
    "jobs",
    "fg",
    "bg",
    "pwd",
    "type",
    "echo",
    "printf",
    "alias",
    "abbr",
    "unabbr",
    "unalias",
    "trap",
    "read",
    "mapfile",
    "readarray",
    "history",
    "wake",
    "wait",
    "kill",
    "z",
    "zi",
    "reload",
    "shift",
    "declare",
    "local",
    "readonly",
    "typeset",
    "shopt",
    "command",
    "builtin",
    "caller",
    "complete",
    "compgen",
    "prevd",
    "nextd",
    "from-json",
    "to-json",
    "where",
    "select",
    "table",
    "exec",
    "time",
    "help",
    "test",
    "[",
    "eval",
    "umask",
    "getopts",
];

/// Command-line flags and subcommands with their own `help TOPIC` entry.
pub const CLI_TOPICS: &[&str] = &[
    "-c",
    "--check",
    "--json",
    "-i",
    "-l",
    "--login",
    "-P",
    "--private",
    "--env",
    "--no-config",
    "--version",
    "--help",
    "-h",
    "state",
    "config explain",
    "capabilities",
    "status",
    "doctor",
    "agent",
    "agent guide",
    "code",
    "completions",
    "parallel",
    "workflow",
    "--stdio",
    "--socket",
    "--mcp",
    "--state-file",
    "--redact-env",
];

/// The index printed by `sharky --help`, `sharky help` and `help`.
pub fn index() -> String {
    let mut out = String::new();
    out.push_str("Usage: sharky [OPTIONS] [FILE]\n");
    out.push_str("       sharky state --json [--env] [--no-config]\n");
    out.push_str("       sharky capabilities --json\n");
    out.push_str("       sharky completions list --json\n");
    out.push_str("       sharky status [--no-config]\n");
    out.push_str("       sharky doctor [--no-config]\n");
    out.push_str("       sharky agent guide [--json]\n");
    out.push_str("       sharky agent --stdio | --socket PATH | --mcp [--no-config] [--redact-env]\n");
    out.push_str("       sharky code [OPTIONS] [FILE]\n");
    out.push_str("       sharky help [TOPIC] [--json]\n");
    out.push('\n');
    out.push_str("  -c COMMAND   run COMMAND and exit\n");
    out.push_str(
        "  --check      check top-level syntax only: no execution, config, profiles or history\n",
    );
    out.push_str("               requires -c or a file; reports line info and exits 2 on errors\n");
    out.push_str("  -i           force interactive mode\n");
    out.push_str("  -l, --login  read login profiles before the command\n");
    out.push_str("  -P, --private  keep no history file: read and write nothing\n");
    out.push_str("  --json       print one JSON object for -c, state, capabilities or help\n");
    out.push_str("  --env        include the environment in state JSON\n");
    out.push_str("  capabilities  print shell, protocol and isolation capabilities as JSON\n");
    out.push_str("  completions  print machine-readable lists (completions list --json)\n");
    out.push_str("  code         run native programmatic Code Mode and print one JSON envelope\n");
    out.push_str("  state        print shell state as JSON (requires --json)\n");
    out.push_str("  status       print version, resolved paths and active feature flags\n");
    out.push_str("  doctor       run read-only local checks; exit 1 if any check fails\n");
    out.push_str("  config explain [TOPIC]  print the configuration guide (or one topic)\n");
    out.push_str("  help parallel  print the agent protocol's parallel-executor contract\n");
    out.push_str("  help [TOPIC]  print this help, or the help for one builtin or subcommand\n");
    out.push_str("  --no-config  do not load the configuration file\n");
    out.push_str("  --version    print the version\n");
    out.push_str("  --help       print this help\n");
    out
}

/// Text for one topic, or `None` when no builtin, flag, subcommand or
/// configuration topic has that id.
///
/// Builtin and CLI text ends with a newline; a configuration topic is returned
/// exactly as `sharky config explain` prints it.
pub fn topic(id: &str) -> Option<String> {
    topic_with_kind(id).map(|(_, text)| text)
}

fn topic_with_kind(id: &str) -> Option<(&'static str, String)> {
    if let Some(text) = builtin_text(id) {
        return Some(("builtin", text.to_string()));
    }
    if let Some(text) = cli_text(id) {
        return Some(("cli", text.to_string()));
    }
    guide::topic(id).map(|text| ("config", text))
}

/// Handle `sharky help [TOPIC] [--json]`. `None` means the invocation is not a
/// help request and normal argument parsing continues.
pub(crate) fn try_cli(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("help") {
        return None;
    }
    let tail = &args[1..];
    if tail.is_empty() {
        print_text(&index());
        return Some(0);
    }

    let (json, topic_args) = if tail == ["--json"] {
        (true, &[][..])
    } else if tail.len() >= 2 && tail.last().map(String::as_str) == Some("--json") {
        (true, &tail[..tail.len() - 1])
    } else if tail.len() >= 2 && tail[0] == "--json" {
        (true, &tail[1..])
    } else {
        (false, tail)
    };

    if json {
        use crate::agent::json::Value;

        if topic_args.is_empty() {
            let mut builtins: Vec<&str> = BUILTINS.to_vec();
            builtins.sort_unstable();
            let payload = Value::Object(vec![
                ("schema_version".to_string(), Value::Int(1)),
                ("kind".to_string(), Value::String("index".to_string())),
                ("text".to_string(), Value::String(index())),
                (
                    "builtins".to_string(),
                    Value::Array(
                        builtins
                            .into_iter()
                            .map(|name| Value::String(name.to_string()))
                            .collect(),
                    ),
                ),
                (
                    "cli_topics".to_string(),
                    Value::Array(
                        CLI_TOPICS
                            .iter()
                            .map(|name| Value::String((*name).to_string()))
                            .collect(),
                    ),
                ),
                (
                    "config_topics".to_string(),
                    Value::Array(
                        guide::INDEX
                            .iter()
                            .map(|(name, _)| Value::String((*name).to_string()))
                            .collect(),
                    ),
                ),
            ]);
            let mut line = payload.encode();
            line.push('\n');
            print_text(&line);
            return Some(0);
        }

        let id = topic_args.join(" ");
        return Some(match topic_with_kind(&id) {
            Some((kind, text)) => {
                let flags = extract_flags_schema(&text);
                let payload = Value::Object(vec![
                    ("schema_version".to_string(), Value::Int(1)),
                    ("kind".to_string(), Value::String(kind.to_string())),
                    ("topic".to_string(), Value::String(id)),
                    ("text".to_string(), Value::String(text)),
                    ("flags".to_string(), Value::Array(flags)),
                ]);
                let mut line = payload.encode();
                line.push('\n');
                print_text(&line);
                0
            }
            None => {
                let payload = Value::Object(vec![
                    ("schema_version".to_string(), Value::Int(1)),
                    (
                        "error".to_string(),
                        Value::Object(vec![
                            ("code".to_string(), Value::String("UNKNOWN_TOPIC".to_string())),
                            (
                                "message".to_string(),
                                Value::String(format!("no help topics match '{id}'")),
                            ),
                        ]),
                    ),
                ]);
                let mut line = payload.encode();
                line.push('\n');
                print_text(&line);
                1
            }
        });
    }

    let id = topic_args.join(" ");
    Some(match topic(&id) {
        Some(text) => {
            print_text(&text);
            0
        }
        None => {
            unknown(&id);
            1
        }
    })
}

/// Handle the shell builtin. The caller has already removed the `help` word.
pub fn builtin(args: &[String]) -> i32 {
    if args.is_empty() {
        print_text(&index());
        return 0;
    }
    let id = args.join(" ");
    match topic(&id) {
        Some(text) => {
            print_text(&text);
            0
        }
        None => {
            unknown(&id);
            1
        }
    }
}

/// Bash reports an unknown `help` topic with status 1; match that status and
/// keep sharky's diagnostic style.
fn unknown(id: &str) {
    eprintln!("sharky: help: no help topics match '{id}'");
}

pub(crate) fn print_text(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

pub(crate) fn builtin_summary(id: &str) -> Option<&'static str> {
    builtin_text(id)
        .and_then(|text| text.lines().next())
        .map(str::trim)
}

fn builtin_text(id: &str) -> Option<&'static str> {
    if !BUILTINS.contains(&id) {
        return None;
    }
    Some(match id {
        ":" => "Do nothing and return success.\n",
        "true" => "Do nothing and return success.\n",
        "false" => "Do nothing and return failure (status 1).\n",
        "cd" => {
            "cd [DIR]  change the current directory.\n\
             With no argument, go to $HOME; `cd -` goes to $OLDPWD and prints it.\n"
        }
        "hash" => {
            "hash [-lr] [-p PATH] [-dt] [NAME ...]  inspect the command-path cache.\n\
             External commands found through PATH are remembered automatically.\n\
             -t prints each cached path, -l prints reusable `hash -p` lines,\n\
             -r clears the table, -p stores PATH for NAME, -d removes NAME.\n"
        }
        "let" => {
            "let EXPRESSION ...  evaluate arithmetic expressions and assign variables.\n\
             Each expression may contain assignments such as x=3+4 or x+=1.\n\
             The status is 0 when the last result is nonzero, 1 when it is zero\n\
             or an arithmetic error occurs.\n"
        }
        "times" => {
            "times  print accumulated user and system CPU time.\n\
             The first line reports the shell, the second line reports its\n\
             waited-for children, in bash's `MmSS.mmmS` format.\n"
        }
        "disown" => {
            "disown [-ahr] [jobspec ...]  remove jobs from the shell's job table.\n\
             With no jobspec, the current job is selected. -a selects every job,\n\
             -r selects only running jobs, and -h keeps a job listed but stops\n\
             forwarding a hangup signal to it.\n"
        }
        "ulimit" => {
            "ulimit [-HS] [-cfnstuv] [LIMIT]  show or set process resource limits.\n\
             -c core, -f file size, -n open files, -s stack, -t CPU seconds,\n\
             -u user processes, -v virtual memory. -H selects the hard limit,\n\
             -S the soft limit; setting raises no hard limit. -a lists all.\n\
             A missing requested resource defaults to file size (-f).\n"
        }
        "pushd" => {
            "pushd [DIR]  push DIR (or swap the top two entries) and change to it.\n\
             pushd +N / -N rotates the Nth directory (counted from left/right) to the top.\n\
             The stack lives in DIRSTACK, with PWD as its visible first entry.\n"
        }
        "popd" => {
            "popd [+N]  remove the top directory (or entry +N) and change to the new top.\n\
             An empty stack or an out-of-range index is an error.\n"
        }
        "dirs" => {
            "dirs [-c] [-p] [-l] [-v]  print the directory stack.\n\
             -c clears DIRSTACK; -p prints one entry per line; -l avoids `~`
             abbreviations; -v prefixes each line with its index.\n"
        }
        "exit" => {
            "exit [STATUS]  leave the shell.\n\
             With no argument, use the status of the last command.\n"
        }
        "export" => {
            "export [-fn] [NAME[=value] ...] or export -p\n\
             Mark variables for the environment of child processes. `export NAME`\n\
             marks an existing variable without changing its value; `export -n`\n\
             removes the attribute and keeps the value; `export -p` lists only the\n\
             exported variables. A plain assignment keeps the current attribute,\n\
             and `unset` removes the name and its attribute.\n"
        }
        "unset" => {
            "unset NAME ...  remove variables and array elements.\n\
             `unset a[i]` clears one element; `unset a` removes the whole name.\n"
        }
        "set" => {
            "set [OPTIONS] [-- ARG ...]  set shell options and positional parameters.\n\
             Flags: -e errexit, -u nounset, -x xtrace, -H histexpand, -C noclobber,\n\
             -o NAME and +o NAME for errexit, nounset, xtrace, histexpand, pipefail,\n\
             noclobber and vi. `set -- a b` replaces the positional parameters.\n"
        }
        "source" => {
            "source FILE [ARG ...]  run FILE in the current shell.\n\
             The positional parameters are replaced while FILE runs.\n"
        }
        "." => ". FILE [ARG ...]  run FILE in the current shell (synonym for source).\n",
        "break" => "break  leave the innermost enclosing loop.\n",
        "continue" => "continue  start the next iteration of the innermost loop.\n",
        "return" => {
            "return [STATUS]  return from a function or a sourced file.\n\
             With no argument, use the status of the last command.\n"
        }
        "jobs" => "jobs  list background and stopped jobs with their job numbers.\n",
        "fg" => "fg [JOB]  bring a job to the foreground.\n",
        "bg" => "bg [JOB]  continue a stopped job in the background.\n",
        "pwd" => "pwd  print the current directory.\n",
        "echo" => {
            "echo [-neE] [ARG ...]  print ARG, separated by spaces, and a newline.\n\
             -n omits the newline, -e interprets escapes (\\\\, \\a, \\b, \\c, \\e,\n\
             \\f, \\n, \\r, \\t, \\v, \\0nnn, \\xHH, \\uHHHH, \\UHHHHHHHH) and -E\n\
             turns them off again. Flags stop at the first word that is not one;\n\
             \\c ends the output. Without -e, escapes are printed literally.\n"
        }
        "printf" => {
            "printf FORMAT [ARG ...]  print ARG converted as FORMAT says.\n\
             FORMAT is reused while arguments remain. Conversions: %s %b %q %c\n\
             %d %i %u %o %x %X %f %F %e %E %g %G and %%; flags - + <space> # 0;\n\
             width and precision, with `*` taking the value from ARG. Escapes in\n\
             FORMAT and in %b are interpreted, `%b` also honours \\c. A missing\n\
             argument counts as zero or the empty string; a bad conversion exits\n\
             1 and an unknown option or no FORMAT exits 2. `-v NAME` stores the\n\
             output in NAME instead of printing it.\n"
        }
        "type" => {
            "type NAME ...  describe how each name resolves.\n\
             Prints alias, function, shell builtin or the resolved path; exits 1\n\
             when any name is not found.\n"
        }
        "alias" => {
            "alias [NAME[=VALUE] ...]  list aliases, or define one.\n\
             With no argument, print every alias.\n"
        }
        "abbr" => {
            "abbr [NAME[=VALUE] ...]  list abbreviations, or define one.\n\
             An abbreviation expands in the line editor on space or Enter.\n"
        }
        "unabbr" => "unabbr NAME ...  remove abbreviations.\n",
        "unalias" => "unalias NAME ...  remove aliases.\n",
        "trap" => {
            "trap [ACTION] SIGNAL ...  run ACTION when a signal arrives.\n\
             `trap -l` lists signal names; `trap` alone prints the installed traps;\n\
             a single `-` resets the named signals.\n"
        }
        "read" => {
            "read [-r] [NAME ...]  read one line from standard input into NAME.\n\
             With no names, store the whole line in REPLY. -r keeps backslashes.\n\
             Splits on IFS; exits 1 at end of input.\n"
        }
        "mapfile" => {
            "mapfile [-t] [ARRAY]  read lines from standard input into an indexed array.\n\
             -t strips the trailing newline; the default array is MAPFILE.\n"
        }
        "readarray" => "readarray [-t] [ARRAY]  synonym for mapfile.\n",
        "history" => {
            "history [search] [--max N] [--reverse] [--show-time] [--null]\n\
             [--case-sensitive] [--contains|--prefix|--exact] [PATTERN]\n\
             search the history entries.\n\
             The default is a case-insensitive substring match, newest first;\n\
             --prefix and --exact change the match mode and --case-sensitive makes\n\
             case significant. --max N limits the result (`--max 0` prints\n\
             nothing), --reverse prints oldest first, --null ends each entry with a\n\
             NUL instead of a newline, and --show-time prints a UTC\n\
             `# YYYY-MM-DD HH:MM:SS` line before every entry that has a timestamp\n\
             (legacy entries have none and print none).\n\
             `history delete --exact PATTERN` removes the exactly matching records\n\
             (case-sensitively) and rewrites the file under the history lock; `-c`\n\
             clears the file.\n\
             Interactive sessions refresh the shared file before every primary\n\
             prompt, so entries another shell appended appear in the editor, the\n\
             autosuggest and this listing at the next prompt without a manual step;\n\
             `history merge` stays as the forced, idempotent full re-read. History\n\
             expansion is guarded by origin: `!!`, `!n`, `!prefix` and the other\n\
             event designs resolve only entries loaded at startup or run by this\n\
             session, never one picked up from another terminal. After a refresh the\n\
             expansion list (Loaded + Local) can be shorter than this listing, and\n\
             `!n` numbers within that expansion list.\n\
             Ghost autosuggest completes the token under the cursor, not just a\n\
             whole-line prefix: it scans history newest first and prefers an exact\n\
             case-sensitive token match over a case-insensitive one. A record whose\n\
             referenced paths no longer resolve is skipped, with relative paths\n\
             resolved against the directory the command ran in; a command whose\n\
             output path did not exist at record time, and legacy records without\n\
             paths, stay eligible. Right or End accepts the ghost and `tide\n\
             autosuggest off` disables it. Ctrl-R ranks a non-empty query by\n\
             substring frecency: hits times an age weight (4 within a day, 2 within\n\
             30 days, 1 beyond, with legacy records counted as old), breaking ties on\n\
             the newer timestamp and then the file position. The newest exact\n\
             case-sensitive prefix match is pinned first so repeated Ctrl-R still\n\
             finds the command just typed, and further presses page through the\n\
             ranked matches. An empty query and ordinary Up/Down recall stay\n\
             chronological.\n"
        }
        "wake" => {
            "wake COMMAND  register a command to run at interactive startup.\n\
             Commands are kept for this shell only; several wake lines append.\n"
        }
        "wait" => "wait [PID ...]  wait for background jobs (or the named pids) to finish.\n",
        "kill" => "kill [-SIGNAL] PID ...  send a signal to processes or jobs.\n",
        "z" => {
            "z [PATTERN]  jump to a directory visited earlier.\n\
             With no argument, go home; `z -` returns to the previous directory.\n"
        }
        "zi" => "zi [PATTERN]  like z, choosing interactively among the matches.\n",
        "reload" => {
            "reload  re-read config.sharky, theme.sharky and profiles.\n\
             Aliases, abbreviations, key bindings and styles are replaced.\n"
        }
        "shift" => {
            "shift [N]  drop the first N positional parameters (default 1).\n\
             Exits 1 when N is larger than the number of parameters.\n"
        }
        "declare" => {
            "declare [-a | -A] NAME[=VALUE] ...  declare variables and arrays.\n\
             -a makes an indexed array, -A an associative array.\n"
        }
        "local" => "local [-a | -A] NAME[=VALUE] ...  declare variables local to a function.\n",
        "readonly" => {
            "readonly [-a | -A] NAME[=VALUE] ...  declare variables with the same\n\
             forms as declare and give them the readonly attribute: a later\n\
             assignment is an error. -p, or no name, lists the readonly ones.\n"
        }
        "typeset" => "typeset [-a | -A] NAME[=VALUE] ...  synonym for declare.\n",
        "shopt" => {
            "shopt [-s | -u] [OPTION]  query or set a shell option.\n\
             Supported options are `globstar` (recursive `**` matching) and\n\
             `autocd` (enter a directory typed as a command).\n\
             `-s` enables an option, `-u` disables it, and `shopt OPTION`\n\
             prints `OPTION<TAB>on|off`.\n"
        }
        "command" => {
            "command [-pVv] NAME [ARG ...]  run NAME while bypassing functions.\n\
             -v prints how each name resolves, -V prints a verbose description,\n\
             -p searches the default path.\n"
        }
        "builtin" => {
            "builtin [NAME [ARG ...]]  run a shell builtin while bypassing functions.\n\
             Exits 1 if NAME is not a shell builtin.\n"
        }
        "caller" => {
            "caller [EXPR]  print call-stack line, subroutine name, and source file.\n\
             Returns 1 when not in a function or sourced file, or EXPR is out of range.\n"
        }
        "complete" => {
            "complete [-pr] [-W LIST] [-F FUNC] [-f|-d|-c|-v|-b|-a] [CMD ...]  register Tab completions.\n\
             Use -p to list specs and -r to remove specs for CMD.\n"
        }
        "compgen" => {
            "compgen [-W LIST] [-F FUNC] [-f|-d|-c|-v|-b|-a] [PREFIX]  generate completion matches.\n\
             Prints matching candidates one per line and returns 0 if any match, 1 otherwise.\n"
        }
        "prevd" => {
            "prevd [N]  step backward N entries in the session directory history.\n"
        }
        "nextd" => {
            "nextd [N]  step forward N entries in the session directory history.\n"
        }
        "from-json" => {
            "from-json [--jsonl]  parse stdin JSON or JSONL into normalized JSON output.\n"
        }
        "to-json" => {
            "to-json [--pretty]  encode stdin lines or JSON into compact or pretty JSON.\n"
        }
        "where" => {
            "where FIELD OP VALUE  filter a JSON array or JSONL stream from stdin.\n\
             Supports ==, !=, >, >=, <, <=, =~, and contains.\n"
        }
        "select" => {
            "select FIELD ...  project selected keys (supporting dotted paths) from stdin JSON.\n"
        }
        "table" => {
            "table  render a JSON array of objects or JSON object from stdin as an aligned box table.\n"
        }
        "exec" => {
            "exec [-a NAME] COMMAND [ARG ...]  replace the shell with COMMAND.\n\
             With no command, apply the redirections and keep the shell running.\n\
             -a sets argv[0] to NAME.\n"
        }
        "time" => {
            "time [-p] PIPELINE  run PIPELINE and report real, user and system time.\n\
             -p selects the POSIX report format.\n"
        }
        "help" => {
            "help [TOPIC]  print the help index, or the text for one topic.\n\
             Topics are builtin names, command-line flags and subcommands, and the\n\
             configuration topics of `sharky config explain`.\n"
        }
        "test" => {
            "test EXPRESSION  evaluate a conditional expression.\n\
             Exits 0 when true, 1 when false and 2 when the expression is malformed.\n\
             Unary tests include -n, -z, -e, -f, -d, -L, -s, -r, -w, -x and -t;\n\
             binary tests include =, !=, <, >, -eq, -ne, -lt, -le, -gt, -ge, -nt,\n\
             -ot and -ef; combine with !, -a, -o and parentheses.\n"
        }
        "[" => {
            "[ EXPRESSION ]  the same test builtin, requiring a closing `]'.\n\
             Runs without looking up `test` in PATH, so a login profile can rely\n\
             on it before PATH is set up.\n"
        }
        "eval" => {
            "eval [ARG ...]  join ARG with spaces and run it as shell code.\n\
             The code runs in the current shell, so assignments, functions and\n\
             exports persist; the status is the last command's. `return' and\n\
             `exit' in the code leave the enclosing function or shell. A syntax\n\
             error reports a diagnostic and exits 2.\n"
        }
        "umask" => {
            "umask [-p] [-S] [MODE]  show or set the file-creation mask.\n\
             MODE is octal (for example 022) or symbolic like chmod's\n\
             (u=rwx,g=rx,o=). The mask is a process attribute, so files the\n\
             shell creates and processes it starts honour it. -S prints the\n\
             symbolic form and -p prefixes a reusable `umask' command. An\n\
             invalid MODE or option exits 1 or 2.\n"
        }
        "getopts" => {
            "getopts OPTSTRING NAME [ARG ...]  parse options into NAME.\n\
             Each call sets NAME to the next option letter, OPTARG to its\n\
             argument when OPTSTRING ends that letter with a colon, and OPTIND\n\
             to the next argument index. A leading colon silences diagnostics:\n\
             an unknown option then sets OPTARG to the letter, and a missing\n\
             argument sets NAME to `:'. Without ARG the positional parameters\n\
             are parsed. Returns 1 at the end of the options.\n"
        }
        _ => return None,
    })
}

fn cli_text(id: &str) -> Option<&'static str> {
    if !CLI_TOPICS.contains(&id) {
        return None;
    }
    Some(match id {
        "-c" => "-c COMMAND  run COMMAND and exit with its status.\n",
        "--check" => {
            "--check  parse -c or a file for syntax only.\n\
             Exits 0 silently on success, or 2 with a line reference on a top-level\n\
             syntax error. Requires -c or a file; nothing is executed and no\n\
             configuration, profiles or history are read. Cannot be combined with\n\
             -i, -l, --json or state.\n"
        }
        "--json" => {
            "--json  print one JSON object for -c or state.\n\
             The object reports schema version, status, stdout, stderr, duration,\n\
             cwd and environment changes.\n"
        }
        "-i" => {
            "-i  force interactive mode.\n\
             Fails when standard input or output is not a terminal.\n"
        }
        "-l" => "-l  short form of --login; read login profiles before the command.\n",
        "--login" => "--login  read login profiles before the command.\n",
        "-P" => "-P  short form of --private; keep no history file.\n",
        "--private" => {
            "--private  keep no history file: read and write nothing.\n"
        }
        "--env" => {
            "--env  include the environment in the state JSON.\n\
             Only valid together with state.\n"
        }
        "--no-config" => "--no-config  do not load the configuration file.\n",
        "--version" => "--version  print the version and exit.\n",
        "--help" => {
            "--help  print the help index and exit.\n\
             -h is a synonym. `sharky help` prints the same index.\n"
        }
        "-h" => "-h  short form of --help; print the help index and exit.\n",
        "state" => {
            "state --json [--env]  print schema version, cwd, last status and pid.\n\
             --env adds the environment; --json is required. Runs before the shell\n\
             starts, so no history is read or written.\n"
        }
        "config explain" => {
            "config explain [TOPIC]  print the configuration guide.\n\
             With no topic, print an index of topic ids; otherwise join the words\n\
             after `explain` with spaces and print that topic. Each parser directive\n\
             has a topic, alongside `tide history`, `files`, `theme` and `overwrite`.\n\
             Unknown or malformed requests exit 2. Runs before the shell starts.\n"
        }
        "capabilities" => {
            "capabilities --json  print machine-readable shell, protocol and isolation\n\
             capabilities as a single JSON object. Runs before the shell starts and\n\
             reads or writes no configuration, profiles or history.\n"
        }
        "status" => {
            "status [--no-config]  print version, resolved paths and active feature flags.\n\
             Never prints environment or configuration values and never writes files.\n\
             --no-config is accepted as a no-op.\n"
        }
        "doctor" => {
            "doctor [--no-config]  run read-only local checks and print OK or FAIL per check.\n\
             Exits 0 when all checks pass and 1 when any fails. --no-config is a no-op.\n"
        }
        "code" => {
            "sharky code (-e PROGRAM | FILE | -) [--cwd DIR] [--timeout-ms MS]\n\
             [--max-steps N] [--max-calls N] [--args JSON] [--no-config] [--redact-env]\n\
             Run a bounded native Code Mode orchestration script and emit one\n\
             `type:\"code\"` JSON object carrying `value` and an unforgeable `audit`\n\
             envelope (`calls`, `failed_calls`, `cancelled_calls`, `first_failure_status`,\n\
             `steps`, `captures`). Child stdout/stderr stay inside captures and never\n\
             leak into the CLI output stream. Available runtime functions:\n\
             `sh(cmd [, opts])`, `run(argv [, opts])`, `parallel(tasks [, opts])`,\n\
             `map_parallel(items, max_concurrency, fn)`, `workflow(spec)`, `state([sections])`,\n\
             `read_capture(capture_id [, opts])`, plus `len`, `str`, `int`, `json_parse`,\n\
             `json_encode` and `range`. Exit status is 0 on completion, 1 on runtime/usage\n\
             error, 2 on syntax/argument error, 124 on timeout, 143 on cancel.\n"
        }
        "agent guide" => {
            "sharky agent guide [--json]  print an onboarding guide for coding agents.\n\
             Summarizes discovery commands, entrypoints (`--json -c`, `code`,\n\
             `agent --stdio`/`--socket`/`--mcp`), ops, Code Mode builtins, recipes,\n\
             and isolation boundaries. `--json` emits a single structured object.\n"
        }
        "completions" => {
            "sharky completions list --json  emit machine-readable lists of builtins,\n\
             CLI subcommands, help topics, configuration topics, agent ops, Code Mode\n\
             builtins, and MCP tools before shell startup.\n"
        }
        "agent" => {
            "sharky agent --stdio | --socket PATH | --mcp [--no-config] [--redact-env] [--state-file PATH]  serve one session.\n\
             `sharky agent --help` (or `-h`) prints this same topic; `sharky agent guide --json`\n\
             prints an onboarding guide with recipes for coding agents.\n\
             Exactly one transport is required: `--stdio` uses JSONL on standard\n\
             input/output; `--socket PATH` binds one Unix stream socket with mode 0600,\n\
             accepts a single client and immediately unlinks the path; `--mcp` runs a\n\
             native JSON-RPC 2.0 MCP server on stdio exposing Sharky tools.\n\
             --state-file PATH persists and restores cwd, exported env, variables, and\n\
             aliases across clean shutdowns.\n\
             --redact-env masks sensitive env/variable values (`TOKEN`, `SECRET`,\n\
             `PASSWORD`, `API_KEY`, etc.) with `<redacted>` in `state` and `env_delta`.\n\
             One JSON request per line; each accepted request gets one primary\n\
             response (`close` and session end add the terminal frames below).\n\
             Every message carries `schema_version` 1; the first response is `ready`,\n\
             which lists the ops (`exec`, `state`, `cancel`, `close`, `parallel`,\n\
             `workflow`, `read_capture`, `code`), features and limits this build honors.\n\
             `exec` evaluates shell code (`command`) or runs direct `argv` in this\n\
             session (set `\"isolated\":true` for a copy-on-write child that never\n\
             mutates or exits the session); `exec` also supports optional `tty`,\n\
             `stream`, cross-session OS `lock`, and opt-in OS `sandbox` (`true` for\n\
             `PR_SET_NO_NEW_PRIVS` + 64 MiB `max_file_bytes` + 60s `cpu_seconds`, or\n\
             `{no_network, read_only_root, max_file_bytes, max_procs, cpu_seconds}`);\n\
             `state` always returns `cwd`, `last_status`, `options`, `pid` and accepts\n\
             optional `include` (`env`, `variables`, `functions`, `aliases`, `traps`,\n\
             `jobs`); `cancel` and `timeout_ms` signal the in-flight\n\
             request's process group; `parallel` fans out independent commands\n\
             (`help parallel`); `workflow` runs a dependency DAG with policies, retries\n\
             and locks (`help workflow`); `code` runs a bounded programmatic orchestration\n\
             script (`help code`); tasks accept `output` projection (`strip_ansi`,\n\
             `grep`, `context`, `head_lines`, `tail_lines`, `max_bytes`) and\n\
             `read_capture` pages raw logs by `capture_id` from the bounded in-memory\n\
             capture store.\n\
             Results carry captured stdout/stderr, real byte counts, `*_truncated`\n\
             flags and `duration_ms`/`duration_us`; invalid UTF-8 switches a stream\n\
             to base64. `close` writes an `ack` with `outcome:\"closed\"`, then a\n\
             final `closed` event with the session status. `exit` and a `set -e`\n\
             abort deliver their result first, then that final `closed` event with a\n\
             null id; the EXIT trap runs once on the way out. Other op errors\n\
             (invalid JSON, a duplicate id, an unknown op, a DAG validation error)\n\
             are reported and the session keeps serving.\n\
             Standard input must be a pipe: shell code never reads the control\n\
             channel, because request stdin arrives as a per-request file on fd 0 and\n\
             the protocol lives on private descriptors. A background job that\n\
             outlives its request keeps writing to an unlinked capture and its output\n\
             is never reported in a later response. Login mode is refused (exit 2)\n\
             so profiles cannot write into the protocol stream, and so is terminal\n\
             input. By default the server is not a sandbox: commands run with the\n\
             caller's privileges unless an explicit per-task `sandbox` is requested.\n\
             At most 8192 ids are remembered for duplicate detection; ids\n\
             older than that are forgotten.\n"
        }
        "parallel" => {
            "help parallel  the `op:\"parallel\"` contract of `sharky agent --stdio`.\n\
             See `help agent` for the session, ops and descriptor rules, and\n\
             `help workflow` for DAG dependencies, retries and named locks.\n\
             REQUEST  {\"schema_version\":1,\"id\":\"p1\",\"op\":\"parallel\",\n\
                       \"policy\":\"wait-all\",\"max_concurrency\":2,\"timeout_ms\":120000,\n\
                       \"tasks\":[{\"id\":\"a\",\"command\":\"git status --short\",\n\
                                  \"timeout_ms\":5000,\"cwd\":\"/repo\",\"stdin\":\"...\"}]}\n\
             `tasks[]` is required and unique by id; `policy` defaults to `wait-all`,\n\
             `max_concurrency` to 4 (bounded by `ready.limits.max_parallel`). The\n\
             group `timeout_ms` is a wall-clock deadline; a task may set its own\n\
             `timeout_ms` from its spawn. Tasks inherit shell state but cannot change\n\
             it; `cwd`, `env` and `stdin` are applied in the child. A task runs its\n\
             command as a fresh `-c` string: errexit is off at task start (a session\n\
             `set -e` does not abort it), but `set -e` inside the task works.\n\
             RESPONSE  one `type:\"parallel\"` line: `tasks[]` in REQUEST ORDER with\n\
             `id`, `status`, `termination`, `state`, `duration_ms`/`duration_us`,\n\
             `cwd`, `stdout`/`stderr` with `*_bytes` and `*_truncated`, plus optional\n\
             `reason`, `env_delta` and `pgid`. The group adds `policy`, `state`,\n\
             `status`, `started_tasks`, `duration_ms`/`duration_us`,\n\
             `max_concurrency` and `output_truncated`. Group `status` is 0 if all\n\
             tasks succeeded, otherwise the status of the first failing task in\n\
             request order. `output_truncated` flags the group byte budget cutting a\n\
             stream; `*_truncated` flags a per-stream cap. There is no streaming: the\n\
             single response arrives when the group ends.\n\
             POLICY  `wait-all` runs every task; `fail-fast` cancels running tasks\n\
             and drops queued ones after the first failure (queued tasks report\n\
             `cancelled`/130 with `reason:\"fail_fast\"`).\n\
             CANCEL  `{\"op\":\"cancel\",\"target\":\"p1\"}` cancels the whole group;\n\
             add `\"task\":\"a\"` to cancel one task while its siblings continue.\n\
             LIMITS  Tasks are isolated interpreter copies over the SAME filesystem:\n\
             no FS sandbox, no locking and no conflict detection. Use separate\n\
             worktrees or directories (and separate `CARGO_TARGET_DIR` for Cargo\n\
             commands). `max_concurrency` bounds tasks, not processes; a background\n\
             job outliving its task writes to an unlinked, unread capture.\n"
        }
        "workflow" => {
            "help workflow  the `op:\"workflow\"` DAG contract of `sharky agent --stdio`.\n\
             See `help agent` for the session rules and `help parallel` for the\n\
             shared task-attempt isolation and capture rules.\n\
             REQUEST  {\"schema_version\":1,\"id\":\"w1\",\"op\":\"workflow\",\n\
                       \"policy\":\"wait-all\",\"failure_policy\":\"skip\",\"max_concurrency\":4,\n\
                       \"tasks\":[{\"id\":\"fmt\",\"command\":\"cargo fmt --check\",\"lock\":\"target\"},\n\
                                  {\"id\":\"test\",\"command\":\"cargo test\",\"depends_on\":[\"fmt\"],\n\
                                   \"condition\":{\"task\":\"fmt\",\"exit_code\":{\"eq\":0}},\n\
                                   \"retry\":{\"max_attempts\":2,\"on\":[\"timeout\",\"spawn_error\"],\"backoff_ms\":500}}]}\n\
             `depends_on` validates against missing refs, duplicates, self-edges and\n\
             cycles (`INVALID_DEPENDENCY`, `WORKFLOW_CYCLE`, `INVALID_CONDITION`) before\n\
             anything spawns. `depends_on` orders execution only: nodes do not inherit\n\
             another node's `cwd`, `env` or streams; every attempt starts from the\n\
             session snapshot.\n\
             POLICY  `policy` (`wait-all` | `fail-fast`) reacts to any real failure.\n\
             `failure_policy` controls dependents of non-zero nodes: `skip` (default,\n\
             `skipped`/125 `reason:\"dependency_failed\"`), `fail` (cancels unfinished\n\
             run -> `blocked`/130 `reason:\"failure_policy\"`), or `continue`.\n\
             RETRY  `max_attempts` defaults to 1 (no retry); `on` defaults to\n\
             `[\"timeout\",\"spawn_error\"]` and allows `\"failure\"`, with `fixed` or\n\
             capped `exponential` backoff (`backoff_ms`). Warning: a retry re-runs the\n\
             command cleanly at the scheduler level only; if a command is not\n\
             idempotent, do not enable `failure` retry.\n\
             CONDITION  One parent-side predicate over one dependency (`exit_code`\n\
             `eq`/`ne`/`in`, `stdout_contains`, `stdout_equals`, `stderr_contains`).\n\
             Text predicates read captured UTF-8 prefixes only; truncated or binary\n\
             streams evaluate false (`skipped`/125 `reason:\"condition_false\"`).\n\
             LOCK  `lock` is a named mutex of capacity 1 enforced both within this\n\
             workflow and across concurrent Sharky agent sessions via an OS\n\
             advisory `flock`. Lock-waiting nodes cost no window slot; held\n\
             locks span retries and backoff. `cancel` accepts `target` and\n\
             optional `task`.\n\
             RESPONSE  one `type:\"workflow\"` line: `order[]` (topological order),\n\
             `tasks[]` in REQUEST ORDER with `attempts` and `attempts_detail[]`, plus\n\
             `started_tasks`, `attempts_total`, `skipped_tasks`, `blocked_tasks` and\n\
             `output_truncated`. Nodes run over the SAME filesystem with no FS\n\
             sandbox.\n"
        }
        "--stdio" => {
            "--stdio  serve the agent JSONL protocol on standard input and output.\n\
             Required by `sharky agent`; standard input must not be a terminal.\n"
        }
        "--socket" => {
            "--socket PATH  serve the agent JSONL protocol on a Unix stream socket.\n\
             Mutually exclusive with `--stdio` and `--mcp`. Binds one private socket,\n\
             accepts a single client, then unlinks the path.\n"
        }
        "--mcp" => {
            "--mcp  serve Sharky tools over JSON-RPC 2.0 (Model Context Protocol) on stdio.\n\
             Mutually exclusive with `--stdio` and `--socket`.\n"
        }
        "--state-file" => {
            "--state-file PATH  restore persisted cwd, exported env, variables and aliases\n\
             on `sharky agent` startup and save them atomically on clean close.\n"
        }
        "--redact-env" => {
            "--redact-env  mask sensitive variable values (`TOKEN`, `SECRET`, `PASSWORD`,\n\
             `API_KEY`, etc.) with `<redacted>` in `sharky agent` `state` and\n\
             `env_delta` responses. Stdout and stderr bytes are not scrubbed.\n"
        }
        _ => return None,
    })
}

fn extract_flags_schema(text: &str) -> Vec<crate::agent::json::Value> {
    use crate::agent::json::Value;

    let mut flags: Vec<(String, bool)> = Vec::new();
    let mut record = |flag: String, takes_value: bool| {
        if let Some(existing) = flags.iter_mut().find(|(name, _)| name == &flag) {
            existing.1 |= takes_value;
        } else {
            flags.push((flag, takes_value));
        }
    };

    let words: Vec<&str> = text.split_whitespace().collect();
    let is_placeholder = |raw: &str| {
        let trimmed = raw.trim_matches(|c: char| matches!(c, '[' | ']' | '(' | ')' | ',' | '.' | '`'));
        !trimmed.is_empty()
            && trimmed
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c == '-')
            && !trimmed.starts_with('-')
            && trimmed != "OR"
    };

    let mut idx = 0;
    while idx < words.len() {
        let raw = words[idx];
        let trimmed = raw.trim_matches(|c: char| matches!(c, '[' | ']' | '(' | ')' | ',' | '.' | '`' | ':'));
        if trimmed.starts_with("---") || trimmed == "--" || trimmed == "-" {
            idx += 1;
            continue;
        }
        let prev = idx.checked_sub(1).and_then(|p| words.get(p).copied()).unwrap_or("");
        let next_is_placeholder = words.get(idx + 1).is_some_and(|w| is_placeholder(w));

        if let Some(body) = trimmed.strip_prefix("--") {
            if !body.is_empty()
                && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && !body.chars().all(|c| c.is_ascii_digit())
            {
                let takes_val = !raw.ends_with(']') && !raw.ends_with(')') && next_is_placeholder;
                record(format!("--{body}"), takes_val);
            }
        } else if let Some(body) = trimmed.strip_prefix('-')
            && !body.is_empty()
            && body.chars().all(|c| c.is_ascii_alphanumeric())
            && !body.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            // Ignore numeric index notation `-N` paired with `+N` in stack docs.
            if body == "N" && prev.contains("+N") {
                idx += 1;
                continue;
            }
            // Split clustered option flags inside synopsis brackets (`[-SHacfnstuv]`, `[-ahr]`).
            if raw.starts_with('[') && body.len() > 1 && !(body == "stdin" || body == "stdio") {
                let chars: Vec<char> = body.chars().collect();
                for (pos, ch) in chars.iter().enumerate() {
                    let is_last = pos + 1 == chars.len();
                    let takes_val = is_last && !raw.ends_with(']') && next_is_placeholder;
                    record(format!("-{ch}"), takes_val);
                }
            } else if body.len() <= 2 {
                let takes_val = !raw.ends_with(']') && !raw.ends_with(')') && next_is_placeholder;
                record(format!("-{body}"), takes_val);
            }
        }
        idx += 1;
    }

    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    flags
        .into_iter()
        .map(|(flag, takes_value)| {
            let desc = lines
                .iter()
                .skip(1)
                .find(|line| line.contains(&flag))
                .or_else(|| lines.first())
                .copied()
                .unwrap_or("")
                .to_string();
            Value::Object(vec![
                ("flag".to_string(), Value::String(flag)),
                ("takes_value".to_string(), Value::Bool(takes_value)),
                ("description".to_string(), Value::String(desc)),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_and_cli_topic_has_text() {
        for name in BUILTINS {
            let text = builtin_text(name).unwrap_or_else(|| panic!("no text for {name}"));
            assert!(!text.trim().is_empty(), "{name} is empty");
        }
        for name in CLI_TOPICS {
            let text = cli_text(name).unwrap_or_else(|| panic!("no text for {name}"));
            assert!(!text.trim().is_empty(), "{name} is empty");
        }
    }

    #[test]
    fn every_configuration_topic_is_reachable_through_help() {
        for (id, _) in guide::INDEX {
            assert!(topic(id).is_some(), "help cannot reach {id}");
        }
    }

    #[test]
    fn unknown_topic_has_no_text() {
        assert!(topic("not-a-topic").is_none());
    }
}
