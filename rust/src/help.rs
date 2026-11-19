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
    "help",
];

/// Command-line flags and subcommands with their own `help TOPIC` entry.
pub const CLI_TOPICS: &[&str] = &[
    "-c",
    "--check",
    "--json",
    "-i",
    "state",
    "config explain",
    "status",
    "doctor",
];

/// The index printed by `sharky --help`, `sharky help` and `help`.
pub fn index() -> String {
    let mut out = String::new();
    out.push_str("Usage: sharky [OPTIONS] [FILE]\n");
    out.push_str("       sharky state --json [--env] [--no-config]\n");
    out.push_str("       sharky status\n");
    out.push_str("       sharky doctor\n");
    out.push_str("       sharky help [TOPIC]\n");
    out.push('\n');
    out.push_str("  -c COMMAND   run COMMAND and exit\n");
    out.push_str(
        "  --check      check top-level syntax only: no execution, config, profiles or history\n",
    );
    out.push_str("               requires -c or a file; reports line info and exits 2 on errors\n");
    out.push_str("  -i           force interactive mode\n");
    out.push_str("  -l, --login  read login profiles before the command\n");
    out.push_str("  -P, --private  keep no history file: read and write nothing\n");
    out.push_str("  --json       print one JSON object for -c or state\n");
    out.push_str("  --env        include the environment in state JSON\n");
    out.push_str("  state        print shell state as JSON (requires --json)\n");
    out.push_str("  status       print version, resolved paths and active feature flags\n");
    out.push_str("  doctor       run read-only local checks; exit 1 if any check fails\n");
    out.push_str("  config explain [TOPIC]  print the configuration guide (or one topic)\n");
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
    if let Some(text) = builtin_text(id) {
        return Some(text.to_string());
    }
    if let Some(text) = cli_text(id) {
        return Some(text.to_string());
    }
    guide::topic(id)
}

/// Handle `sharky help [TOPIC]`. `None` means the invocation is not a help
/// request and normal argument parsing continues.
pub(crate) fn try_cli(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("help") {
        return None;
    }
    let tail = &args[1..];
    if tail.is_empty() {
        print_text(&index());
        return Some(0);
    }
    let id = tail.join(" ");
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

fn print_text(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
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
        "exit" => {
            "exit [STATUS]  leave the shell.\n\
             With no argument, use the status of the last command.\n"
        }
        "export" => {
            "export NAME=value ...  set variables for this shell and its children.\n\
             Only assignments are accepted; names without = are ignored.\n"
        }
        "unset" => {
            "unset NAME ...  remove variables and array elements.\n\
             `unset a[i]` clears one element; `unset a` removes the whole name.\n"
        }
        "set" => {
            "set [OPTIONS] [-- ARG ...]  set shell options and positional parameters.\n\
             Flags: -e errexit, -u nounset, -x xtrace, -H histexpand, -o NAME and\n\
             +o NAME for errexit, nounset, xtrace, histexpand, pipefail and vi.\n\
             `set -- a b` replaces the positional parameters.\n"
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
            "history [ -c | merge ]  print the history entries.\n\
             `-c` clears the history file; `merge` picks up entries other shells\n\
             appended since this shell started.\n"
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
             forms as declare.\n"
        }
        "typeset" => "typeset [-a | -A] NAME[=VALUE] ...  synonym for declare.\n",
        "shopt" => {
            "shopt [-s | -u] [OPTION]  query or set a shell option.\n\
             Only globstar is supported: `-s` enables recursive ** matching, `-u`\n\
             disables it, and `shopt globstar` prints `globstar<TAB>on|off`.\n"
        }
        "command" => {
            "command [-pVv] NAME [ARG ...]  run NAME while bypassing functions.\n\
             -v prints how each name resolves, -V prints a verbose description,\n\
             -p searches the default path.\n"
        }
        "help" => {
            "help [TOPIC]  print the help index, or the text for one topic.\n\
             Topics are builtin names, command-line flags and subcommands, and the\n\
             configuration topics of `sharky config explain`.\n"
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
        "status" => {
            "status  print version, resolved paths and active feature flags.\n\
             Never prints environment or configuration values and never writes files.\n"
        }
        "doctor" => {
            "doctor  run read-only local checks and print OK or FAIL per check.\n\
             Exits 0 when all checks pass and 1 when any fails.\n"
        }
        _ => return None,
    })
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
