use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::libc::_exit;
use nix::sys::signal::{SigHandler, Signal, killpg, signal};
use nix::sys::resource::{RLIM_INFINITY, Resource, UsageWho, getrlimit, getrusage, setrlimit};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{
    ForkResult, Pid, close, fork, getpid, getpgrp, pipe2, setpgid, tcsetpgrp,
};

use sharky_config::{self as config, Config};
use crate::ast::{
    AndOr, Assignment, AssignmentValue, CaseArm, CaseTerm, Command, Entry, List, Pipeline, Program,
    Redirect, RedirectOp, Simple, literal,
};
use crate::arith;
use crate::exec;
use crate::history::History;
use crate::history::Entry as HistoryEntry;
use crate::test_expr;
use crate::test_posix;
use crate::z::Z;
use crate::lexer::{self, ParamOp, Parameter, Part, TokenKind, Word};
use crate::parser;

#[derive(Clone, Default)]
struct CompletionSpec {
    wordlist: Vec<String>,
    function: Option<String>,
    actions: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct ShellSnapshot {
    cwd: PathBuf,
    env: Vec<(String, String)>,
    exported: HashSet<String>,
    readonly_vars: HashSet<String>,
    arrays: HashMap<String, Vec<Option<String>>>,
    assoc: HashMap<String, BTreeMap<String, String>>,
    namerefs: HashMap<String, Option<String>>,
    int_vars: HashSet<String>,
    lower_vars: HashSet<String>,
    upper_vars: HashSet<String>,
    funcs: HashMap<String, List>,
    traps: HashMap<String, String>,
    positional: Vec<String>,
    aliases: HashMap<String, String>,
    abbreviations: HashMap<String, String>,
    errexit: bool,
    nounset: bool,
    xtrace: bool,
    pipefail: bool,
    noclobber: bool,
    errtrace: bool,
    functrace: bool,
    globstar: bool,
    nullglob: bool,
    dotglob: bool,
    failglob: bool,
    nocasematch: bool,
    extglob: bool,
}

pub struct Shell {
    pub env: Vec<(String, String)>,
    /// Names carrying bash's export attribute. `env` stays the single value
    /// store; only names listed here are copied into a child environment.
    exported: HashSet<String>,
    /// Names carrying bash's readonly attribute. A refused plain assignment is
    /// fatal in a non-interactive shell; every other write reports and goes on.
    readonly_vars: HashSet<String>,
    /// The level `SHLVL` reports. It starts at the inherited value plus one and
    /// follows every assignment, so the no-fork final-command path can export
    /// one less without touching the shell's own value.
    shlvl_level: i64,
    /// The simple command bash would exec without forking at the end of a `-c`
    /// string, a command substitution or a subshell. That path exports SHLVL
    /// one lower than the shell reports.
    shlvl_tail: Option<*const Simple>,
    mark_tail_on_eval: bool,
    /// Set in a forked pipeline member. Bash skips the no-fork exec path for a
    /// command or process substitution created there, so its child exports the
    /// shell level unchanged.
    in_pipeline_member: bool,
    pub cwd: PathBuf,
    /// The shell's own process, as `$$` reports it. A forked child keeps this
    /// value, the way bash reports the parent shell from a subshell.
    pid: Pid,
    pub last_status: i32,
    pub last_duration_ms: u64,
    /// Fine-grained counterpart of `last_duration_ms`, read from the same
    /// `Instant` so `duration_ms` is just `duration_us / 1000`.
    pub last_duration_us: u64,
    funcs: HashMap<String, List>,
    arrays: HashMap<String, Vec<Option<String>>>,
    assoc: HashMap<String, BTreeMap<String, String>>,
    namerefs: HashMap<String, Option<String>>,
    int_vars: HashSet<String>,
    lower_vars: HashSet<String>,
    upper_vars: HashSet<String>,
    positional: Vec<String>,
    aliases: HashMap<String, String>,
    abbreviations: HashMap<String, String>,
    wake: Vec<String>,
    keybinds: HashMap<String, String>,
    settings: HashMap<String, String>,
    script: Option<String>,
    bash_source: Vec<String>,
    funcname: Vec<String>,
    bash_lineno: Vec<usize>,
    func_sources: HashMap<String, String>,
    current_line: usize,
    // Set only for a `-c` command string. Bash reports a null or unset
    // parameter error with 127 there, but with 1 when the same text arrives as
    // a script file, on standard input, in a subshell or interactively.
    command_string: bool,
    interactive: bool,
    /// Agent session: a request-scoped `setpgid` gives every foreground child
    /// its own process group so a cancel can signal the request, never the
    /// agent. Never consulted by the REPL, `-c` or a script (both fields stay
    /// at their defaults there).
    isolate_groups: bool,
    /// Agent session: `exec` refuses to replace the interpreter, because the
    /// protocol would die mid-response. See [`Shell::session_eval`].
    session_mode: bool,
    errexit: bool,
    nounset: bool,
    xtrace: bool,
    pipefail: bool,
    noclobber: bool,
    errtrace: bool,
    functrace: bool,
    errexit_suppressed: bool,
    nounset_failed: bool,
    // ${name?word} and ${name:?word} failures, which bash reports with 127 for
    // a -c command string but with 1 in a script, a subshell, an interactive
    // shell or a shell with errexit enabled.
    null_parameter_error: bool,
    in_subshell: bool,
    traps: HashMap<String, String>,
    exit_trap_ran: bool,
    /// Set by an EXIT action installed in this shell, so a task child knows the
    /// action is its own even when the text matches the inherited one.
    exit_trap_defined_here: bool,
    // Guards the trap path: an action is shell code, so it can call `trap`,
    // and a signal delivered while an action runs is kept for the next safe
    // point instead of nesting.
    trap_running: bool,
    // Set when a trap action asked to exit while the run loop was waiting for a
    // foreground job, where there is no flow value to return.
    trap_exit: Option<i32>,
    lineno_offset: Option<i64>,
    history: Option<History>,
    history_load_error: Option<String>,
    histexpand: bool,
    vi: bool,
    globstar: bool,
    nullglob: bool,
    dotglob: bool,
    failglob: bool,
    nocasematch: bool,
    extglob: bool,
    autocd: Option<bool>,
    random_state: std::cell::Cell<u32>,
    seconds_origin: std::time::Instant,
    seconds_offset: i64,
    z: Option<Z>,
    config_dir: Option<PathBuf>,
    substitution_fds: Vec<i32>,
    substitution_pids: Vec<Pid>,
    substitution_paths: Vec<String>,
    substitution_ready: Vec<i32>,
    json_exec: Option<(RawFd, Vec<(String, String)>)>,
    expansion_error: bool,
    substitution_status: Option<i32>,
    pub exit_requested: Option<i32>,
    jobs: Vec<Job>,
    /// Statuses of background jobs that already left `jobs`. Bash keeps a dead
    /// job in its table until `wait` with no arguments runs, so a `wait` that
    /// arrives after the Done notice still reports the real status.
    finished_jobs: Vec<FinishedJob>,
    next_job: usize,
    job_pgid: Option<Pid>,
    last_bg_pid: Option<Pid>,
    colors: HashMap<String, String>,
    styles: HashMap<String, String>,
    local_scopes: Vec<Vec<SavedLocal>>,
    // Nesting depth of `source`/`.` calls. Bash lets `return` leave only a
    // function or a sourced file, so this is the second context that makes the
    // builtin legal; zero at the top level of a script or `-c` string.
    source_depth: usize,
    // `getopts` keeps its scan position between calls: the argv index (1-based,
    // with $0 first) and the position inside a clustered word, the way bash's
    // sh_getopt does. The last OPTIND written lets a user assignment be told
    // apart from the shell's own update.
    getopts_optind: usize,
    getopts_pending: Option<(usize, usize)>,
    getopts_written: Option<String>,
    // Set by any assignment to OPTIND, so a write that repeats the current
    // value still resets the scan the way bash's variable hook does.
    getopts_dirty: bool,
    /// Paths remembered by `hash` and ordinary PATH lookups. Per-shell, so a
    /// new shell and `command -p` never see another shell's cache.
    pub(crate) command_cache: exec::CommandCache,
    highlight_cache: std::cell::RefCell<HashMap<String, bool>>,
    hook_running: bool,
    dir_history: Vec<PathBuf>,
    dir_history_index: usize,
    completions: HashMap<String, CompletionSpec>,
}

struct SavedLocal {
    name: String,
    var: Option<String>,
    array: Option<Vec<Option<String>>>,
    assoc: Option<BTreeMap<String, String>>,
    exported: bool,
    readonly: bool,
    nameref: Option<Option<String>>,
    int_var: bool,
    lower_var: bool,
    upper_var: bool,
}

/// The value side of a binding, without its attributes. A `declare -g` write
/// moves the visible value aside so the write lands on the saved global one.
struct BindingValue {
    var: Option<String>,
    array: Option<Vec<Option<String>>>,
    assoc: Option<BTreeMap<String, String>>,
}

/// A `declare -g` write that has to step around a local shadowing the name:
/// the frame holds the global binding while the scope holds the local one.
struct GlobalSwap {
    frame: usize,
    position: usize,
    local: BindingValue,
}

struct Job {
    id: usize,
    pgid: Pid,
    pids: Vec<Pid>,
    command: String,
    /// The signal that stopped the job, kept so `wait` can report 128+signal
    /// after the stop status itself has already been consumed.
    stop: Option<i32>,
    /// Set by `disown -h`: the job remains visible to `jobs` but is removed
    /// from the process-group list the hangup handler signals.
    no_hup: bool,
}

/// A background job that left the job table but whose status a later `wait`
/// may still ask for, by pid or by `%id`. The oldest entry is dropped once
/// [`FINISHED_JOB_LIMIT`] entries exist, so a script that starts thousands of
/// jobs does not grow the shell's memory without bound while the jobs bash
/// would still answer for (`$!` and the last few) stay covered.
struct FinishedJob {
    id: usize,
    pids: Vec<Pid>,
    status: i32,
}

/// How many finished jobs keep their status. Bash bounds its own job table by
/// `CHILD_MAX`, which is 1024 on Linux; the shell only promises the statuses a
/// script can still name, so a smaller bound with a documented eviction rule
/// keeps the common cases exact without unbounded growth.
const FINISHED_JOB_LIMIT: usize = 128;

enum Flow {
    Status(i32),
    Exit(i32),
    // A fatal error in a command abandons the rest of the input unit (the line)
    // it runs on without leaving the shell, as bash discards the line it is
    // executing. It unwinds to `run_program`; a forked child maps it to its
    // exit status.
    Discard(i32),
    // A SIGINT delivered while the shell was running builtin commands. Bash
    // discards the input unit being executed, so the whole construct unwinds to
    // the top level and reports 130.
    Interrupt,
    Break,
    Continue,
    Return(i32),
}

struct Prepared {
    argv: Vec<String>,
    actions: Vec<Action>,
    env: Vec<(String, String)>,
    has_env: bool,
    declarations: Vec<Assignment>,
}

/// One name resolved through the shell's precedence table, shared by `type`
/// and `command` so both report the same answer.
enum Resolution {
    Alias(String),
    Function,
    Builtin,
    External(PathBuf),
}

/// The child side of `spawn`: where its standard streams go, which process
/// group it joins, and the program already resolved for it.
#[derive(Default)]
struct SpawnChild<'a> {
    stdin: Option<RawFd>,
    stdout: Option<RawFd>,
    leader: Option<Pid>,
    /// A caller-chosen program (for example `command -p`) must be exec'd as
    /// given. Re-resolving it in the child would search the current PATH.
    program: Option<&'a Path>,
}

enum Action {
    // The flag asks for noclobber semantics: an existing regular file must not
    // be truncated. `>|` and heredoc temp files always pass false.
    Write(i32, PathBuf, bool),
    Append(i32, PathBuf),
    Read(i32, PathBuf),
    Dup { from: i32, to: i32 },
    Close(i32),
    Here { fd: i32, file: std::fs::File },
}

impl Action {
    // The descriptor this action moves and, for a dup, the one it copies.
    fn fds(&self) -> (RawFd, Option<RawFd>) {
        match self {
            Action::Write(fd, _, _)
            | Action::Append(fd, _)
            | Action::Read(fd, _)
            | Action::Here { fd, .. } => (*fd, None),
            Action::Dup { from, to } => (*to, Some(*from)),
            Action::Close(fd) => (*fd, None),
        }
    }
}

impl Shell {
    pub fn new() -> Self {
        // Rust ignores SIGPIPE at startup, which leaves a write to a pipe with
        // no reader returning EPIPE to code that can only panic. A shell must
        // behave like bash instead: the default disposition ends the process
        // with 128+SIGPIPE (141), so `sharky -c '...' | head -1` stops quietly
        // on the first write nobody is reading.
        unsafe {
            let _ = signal(Signal::SIGPIPE, SigHandler::SigDfl);
        }

        // An untrapped fatal signal still runs the EXIT action before the shell
        // leaves, as bash does, so these are noticed instead of taking their
        // default action. The interactive shell replaces INT with its line
        // editor action when it installs its own handlers.
        for number in [
            nix::libc::SIGINT,
            nix::libc::SIGTERM,
            nix::libc::SIGHUP,
        ] {
            crate::signal::fatal(number);
        }

        let mut env_warned = false;
        let mut env: Vec<(String, String)> = std::env::vars_os()
            .map(|(key, value)| {
                let kind = if crate::cli::is_non_utf8(&key) {
                    Some("environment variable name")
                } else if crate::cli::is_non_utf8(&value) {
                    Some("environment variable")
                } else {
                    None
                };
                if let Some(kind) = kind {
                    if !env_warned {
                        crate::cli::warn_non_utf8(kind);
                        env_warned = true;
                    }
                }
                (
                    key.to_string_lossy().into_owned(),
                    String::from_utf8_lossy(&value.into_vec()).into_owned(),
                )
            })
            .collect();

        // Every inherited variable starts exported, exactly like a variable
        // that arrived from the parent environment.
        let mut exported: HashSet<String> = env.iter().map(|(name, _)| name.clone()).collect();

        // Bash starts one level above the inherited value, treating an unset or
        // unparseable value as 0, and resets a level that would reach 1000.
        let inherited = env
            .iter()
            .find(|(name, _)| name == "SHLVL")
            .map(|(_, value)| value.trim().parse::<i64>().unwrap_or(0))
            .unwrap_or(0);
        let mut shlvl_level = inherited.saturating_add(1);
        if shlvl_level >= 1000 {
            eprintln!("sharky: warning: shell level ({shlvl_level}) too high, resetting to 1");
            shlvl_level = 1;
        }
        set_env_entry(&mut env, "SHLVL", shlvl_level.to_string());
        exported.insert("SHLVL".to_string());

        // `_` names the shell itself before the first command. An inherited
        // value is kept as it arrived; it is only ever exported to children as
        // the resolved command path.
        if !env.iter().any(|(name, _)| name == "_") {
            let name = std::env::args_os()
                .next()
                .map(|arg| arg.to_string_lossy().into_owned())
                .filter(|arg| !arg.is_empty())
                .unwrap_or_else(|| "sharky".to_string());
            env.push(("_".to_string(), name));
        }

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        crate::cli::warn_if_non_utf8(cwd.as_os_str(), "working directory");
        let mut shell = Shell {
            env,
            exported,
            shlvl_level,
            shlvl_tail: None,
            mark_tail_on_eval: false,
            in_pipeline_member: false,
            cwd: cwd.clone(),
            pid: getpid(),
            last_status: 0,
            last_duration_ms: 0,
            last_duration_us: 0,
            funcs: HashMap::new(),
            arrays: HashMap::new(),
            assoc: HashMap::new(),
            namerefs: HashMap::new(),
            int_vars: HashSet::new(),
            lower_vars: HashSet::new(),
            upper_vars: HashSet::new(),
            positional: Vec::new(),
            aliases: HashMap::new(),
            abbreviations: HashMap::new(),
            wake: Vec::new(),
            keybinds: HashMap::new(),
            settings: HashMap::new(),
            script: None,
            bash_source: Vec::new(),
            funcname: Vec::new(),
            bash_lineno: Vec::new(),
            func_sources: HashMap::new(),
            current_line: 1,
            command_string: false,
            interactive: false,
            isolate_groups: false,
            session_mode: false,
            errexit: false,
            nounset: false,
            xtrace: false,
            pipefail: false,
            noclobber: false,
            errtrace: false,
            functrace: false,
            errexit_suppressed: false,
            nounset_failed: false,
            null_parameter_error: false,
            in_subshell: false,
            traps: HashMap::new(),
            exit_trap_ran: false,
            exit_trap_defined_here: false,
            trap_running: false,
            trap_exit: None,
            lineno_offset: None,
            history: None,
            history_load_error: None,
            histexpand: true,
            vi: false,
            globstar: false,
            nullglob: false,
            dotglob: false,
            failglob: false,
            nocasematch: false,
            extglob: true,
            autocd: None,
            random_state: std::cell::Cell::new(initial_random_seed()),
            seconds_origin: std::time::Instant::now(),
            seconds_offset: 0,
            z: None,
            config_dir: None,
            substitution_fds: Vec::new(),
            substitution_pids: Vec::new(),
            substitution_paths: Vec::new(),
            substitution_ready: Vec::new(),
            json_exec: None,
            expansion_error: false,
            substitution_status: None,
            exit_requested: None,
            readonly_vars: HashSet::new(),
            jobs: Vec::new(),
            finished_jobs: Vec::new(),
            next_job: 1,
            job_pgid: None,
            last_bg_pid: None,
            colors: HashMap::new(),
            styles: HashMap::new(),
            local_scopes: Vec::new(),
            source_depth: 0,
            getopts_optind: 0,
            getopts_pending: None,
            getopts_written: None,
            getopts_dirty: false,
            command_cache: exec::CommandCache::new(),
            highlight_cache: std::cell::RefCell::new(HashMap::new()),
            hook_running: false,
            dir_history: vec![cwd.clone()],
            dir_history_index: 0,
            completions: HashMap::new(),
        };
        shell.set_var("PWD", cwd.display().to_string());
        // Bash creates PWD exported and OLDPWD exported but unset at startup.
        shell.exported.insert("PWD".to_string());
        shell.exported.insert("OLDPWD".to_string());
        shell
    }

    pub fn attach_json_exec(&mut self, fd: RawFd, before: Vec<(String, String)>) {
        self.json_exec = Some((fd, before));
    }

    fn publish_json_exec(&self) {
        if let Some((fd, before)) = &self.json_exec {
            crate::cli::write_delta_fd(*fd, self, before);
        }
    }

    pub fn load_config(&mut self, no_config: bool) {
        if no_config {
            return;
        }

        let Some(dir) = config_dir() else {
            return;
        };

        ensure_config_layout(&dir);
        self.config_dir = Some(dir.clone());
        match config::load(&dir) {
            Ok(config) => self.apply_config(config),
            Err(err) => eprintln!("sharky: config error in {err}"),
        }
        self.discover_profile_paths();
    }

    pub fn discover_profile_paths(&mut self) {
        if self.setting("profile_paths") == Some("off") {
            return;
        }
        let home = PathBuf::from(self.var("HOME"));
        if home.as_os_str().is_empty() {
            return;
        }
        for dir in crate::profiles::discover(&home) {
            self.append_path(&dir);
        }
    }

    pub(crate) fn apply_config(&mut self, config: Config) {
        for (name, value) in config.env {
            self.set_var(&name, value);
            // A config `pearl` line promises the variable to the shell and its
            // children, so it carries the export attribute.
            self.mark_exported(&name);
        }
        for dir in config.paths {
            self.add_path(&dir);
        }
        self.aliases.extend(config.aliases);
        self.abbreviations.extend(config.abbreviations);
        self.wake.extend(config.wake);
        self.keybinds.extend(config.keybinds);
        self.settings.extend(config.settings);
        self.colors.extend(config.colors);
        self.styles.extend(config.styles);
    }

    fn append_path(&mut self, dir: &str) {
        let current = self.var("PATH");
        if current.split(':').any(|entry| entry == dir) {
            return;
        }
        let value = if current.is_empty() {
            dir.to_string()
        } else {
            format!("{current}:{dir}")
        };
        self.set_var("PATH", value);
    }

    fn add_path(&mut self, dir: &str) {
        let expanded = match dir.strip_prefix("~/") {
            Some(rest) => {
                let home = self.var("HOME");
                if home.is_empty() {
                    dir.to_string()
                } else {
                    format!("{home}/{rest}")
                }
            }
            None => dir.to_string(),
        };

        let current = self.var("PATH");
        if current.split(':').any(|entry| entry == expanded) {
            return;
        }
        let value = if current.is_empty() {
            expanded
        } else {
            format!("{expanded}:{current}")
        };
        self.set_var("PATH", value);
    }

    pub fn run_script(&mut self, path: &str, args: &[String]) -> i32 {
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(err) => {
                eprintln!("sharky: {path}: {err}");
                return 127;
            }
        };

        self.script = Some(path.to_string());
        self.positional = args.to_vec();
        self.bash_source.insert(0, path.to_string());
        self.sync_call_stack_arrays();
        let status = self.run(&source);
        if !self.bash_source.is_empty() {
            self.bash_source.remove(0);
        }
        self.sync_call_stack_arrays();
        status
    }

    pub fn run(&mut self, source: &str) -> i32 {
        // Only the top-level `-c` string gets bash's no-fork final-command
        // path; profiles, traps, scripts and stdin do not.
        let saved_tail = self.shlvl_tail;
        self.shlvl_tail = None;
        let mark = self.command_string && !self.exit_trap_ran;
        let result = if mark {
            self.eval_tail(source)
        } else {
            self.eval(source)
        };
        let status = match result {
            Ok(code) => code,
            Err(err) => {
                let prefix = self.script.as_deref().unwrap_or("sharky");
                eprintln!("{}", err.format_diagnostic(prefix));
                2
            }
        };
        // The parsed program is gone, so the tail pointer is cleared before
        // the EXIT trap runs and restored only for an enclosing input unit.
        self.shlvl_tail = None;
        let status = self.finish(status);
        self.shlvl_tail = saved_tail;
        self.close_substitutions_from(0);
        status
    }

    fn eval_tail(&mut self, source: &str) -> Result<i32, parser::ParseError> {
        self.mark_tail_on_eval = true;
        let result = self.eval(source);
        self.mark_tail_on_eval = false;
        result
    }

    /// A process substitution child. Bash applies its no-fork SHLVL adjustment
    /// there unless the substitution was created inside a pipeline member.
    fn eval_substitution(&mut self, source: &str) -> Result<i32, parser::ParseError> {
        if self.in_pipeline_member {
            self.shlvl_tail = None;
            self.eval(source)
        } else {
            self.eval_tail(source)
        }
    }

    pub fn finish(&mut self, status: i32) -> i32 {
        if self.exit_trap_ran {
            return status;
        }
        self.exit_trap_ran = true;

        let Some(command) = self.traps.get("EXIT").cloned() else {
            return status;
        };
        if command.trim().is_empty() {
            return status;
        }

        // `$?` inside the EXIT action is the status the shell is leaving with.
        self.last_status = status;
        // A fatal signal that brought us here must not abort its own cleanup:
        // the flag has done its job once the action starts.
        crate::signal::clear_fatal();
        let saved = self.exit_requested;
        self.exit_requested = None;
        let _ = self.run(&command);
        if let Some(code) = self.exit_requested.take() {
            return code;
        }
        self.exit_requested = saved;
        status
    }

    /// Runs the EXIT action a parallel task defined for itself, once, at task
    /// end. `inherited` is the action the forked child started with: an
    /// unchanged action belongs to the session and must not fire once per task
    /// (design §1.10/§7), while an action the task installed runs even when its
    /// text is identical to the inherited one, as bash does.
    pub(crate) fn finish_task(&mut self, status: i32, inherited: Option<&str>) -> i32 {
        if self.exit_trap_defined_here || self.snapshot_exit_trap().as_deref() != inherited {
            self.finish(status)
        } else {
            status
        }
    }

    pub fn eval(&mut self, source: &str) -> Result<i32, parser::ParseError> {
        self.eval_with(source, || {})
    }

    /// Runs `source` one complete statement at a time, executing each as soon
    /// as it is parsed.
    ///
    /// Bash drives `eval` and startup files this way, so a syntax error in a
    /// later statement reports status 2 without discarding the statements above
    /// it; they have already run. The returned flow is the last statement's, or
    /// an early `return`, `break`, `continue`, `exit` or interrupt.
    fn eval_units(&mut self, source: &str) -> Result<Flow, parser::ParseError> {
        let mut units = match parser::Units::new(source) {
            Ok(units) => units,
            Err(err) => {
                // A lexical error (an unterminated quote or here-document) ends
                // tokenizing before any unit exists, but bash has already run
                // the complete statements above the offending line.
                self.replay_units_before_line(source, err.line);
                return Err(err);
            }
        };

        let mut ran = false;
        while let Some(unit) = units.next_unit() {
            let program = unit?;
            ran = true;
            match self.run_program(&program) {
                Flow::Status(_) => {}
                // A fatal error abandons only the input unit it runs in: bash
                // discards the line it is executing and reads the next one,
                // keeping the failed unit's status for `$?`.
                Flow::Discard(code) => self.last_status = code,
                other => return Ok(other),
            }
        }

        if ran {
            Ok(Flow::Status(self.last_status))
        } else {
            // An empty or comment-only string is a success, whatever `$?` was.
            Ok(Flow::Status(0))
        }
    }

    /// Executes the whole lines above `line`, used when a lexical error aborts
    /// tokenizing before the parser can split the rest into units.
    fn replay_units_before_line(&mut self, source: &str, line: usize) {
        if line <= 1 {
            return;
        }
        let mut seen = 1;
        let mut end = None;
        for (offset, char) in source.char_indices() {
            if char == '\n' {
                seen += 1;
                if seen == line {
                    end = Some(offset + 1);
                    break;
                }
            }
        }
        let Some(end) = end else {
            return;
        };
        let prefix = &source[..end];
        if prefix.trim().is_empty() {
            return;
        }
        // The prefix has its own statements and its own errors; only the
        // original error is reported by the caller.
        let _ = self.eval_units(prefix);
    }

    /// Executes a startup file statement by statement, the way bash reads a
    /// profile: the complete statements above a syntax error have run by the
    /// time it is reported.
    pub fn eval_incremental(&mut self, source: &str) -> Result<(), parser::ParseError> {
        self.eval_units(source).map(|_| ())
    }

    pub fn eval_with(
        &mut self,
        source: &str,
        before_run: impl FnOnce(),
    ) -> Result<i32, parser::ParseError> {
        let program = parser::parse(source)?;
        if std::mem::take(&mut self.mark_tail_on_eval) {
            self.shlvl_tail = final_simple(&program);
        }
        before_run();
        Ok(match self.run_program(&program) {
            Flow::Status(code) | Flow::Exit(code) | Flow::Discard(code) | Flow::Return(code) => code,
            Flow::Interrupt => {
                self.last_status = 130;
                130
            }
            Flow::Break | Flow::Continue => 0,
        })
    }

    pub fn histexpand(&self) -> bool {
        self.histexpand
    }

    pub fn vi_mode(&self) -> bool {
        self.vi
    }

    pub fn set_interactive(&mut self, interactive: bool) {
        self.interactive = interactive;
    }

    /// Marks the source as a `-c` command string, which changes the status bash
    /// reports for a null or unset parameter error.
    pub fn set_command_args(&mut self, script: Option<String>, positional: Vec<String>) {
        if let Some(script) = script {
            self.script = Some(script);
        }
        self.positional = positional;
    }

    pub fn set_command_string(&mut self, command_string: bool) {
        self.command_string = command_string;
    }

    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(String::as_str)
    }

    pub fn variable_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.env.iter().map(|(key, _)| key.clone()).collect();
        names.extend(self.arrays.keys().cloned());
        names.extend(self.assoc.keys().cloned());
        names.extend(self.exported.iter().cloned());
        names
    }
}

// The session surface is consumed by the agent session driver
// (`crate::agent`). Two accessors are read only by the unit tests below, so they
// carry their own allowance instead of hiding the whole surface.
impl Shell {
    // ---- Agent session surface -------------------------------------------
    //
    // The session driver (crate::agent) drives this shell in process, so it
    // needs an evaluation entry point that does not finish the shell, a way to
    // ask for request-scoped process groups, and read-only snapshots for the
    // `state` op. Nothing here changes an existing mode: every flag defaults to
    // the behavior the REPL, `-c`, scripts and batch stdin have always had.

    /// Evaluates one request's source the way the REPL evaluates a line:
    /// parse, run, keep a parse error for the caller. It deliberately does not
    /// do what [`Shell::run`] does for an input unit — no `finish`, no EXIT
    /// trap, no `shlvl_tail` bookkeeping — so the session's EXIT action runs
    /// exactly once, when the driver finally calls `finish`.
    pub(crate) fn session_eval(&mut self, source: &str) -> Result<i32, parser::ParseError> {
        let started = std::time::Instant::now();
        let outcome = self.eval_with(source, || {});
        let elapsed = started.elapsed();
        self.last_duration_ms = elapsed.as_millis() as u64;
        self.last_duration_us = elapsed.as_micros() as u64;
        outcome
    }

    /// Executes an argument vector directly via `PATH` resolution and `execve`
    /// without shell parsing, globbing, expansion, or builtin dispatch.
    pub(crate) fn session_exec_argv(&mut self, argv: &[String]) -> i32 {
        let Some(program) = argv.first() else {
            return 0;
        };
        let started = std::time::Instant::now();
        let env = self.child_env();
        let status = match exec::resolve(program, self) {
            None => {
                let line = format!("sharky: command not found: {program}\n");
                let _ = crate::term::write_stderr(&line);
                127
            }
            Some(path) => {
                let mask = crate::signal::block_all();
                // SAFETY: the child only runs signal/pgid setup and `execve`.
                match unsafe { fork() } {
                    Ok(ForkResult::Parent { child }) => {
                        crate::signal::restore_mask(mask);
                        if (self.interactive || self.isolate_groups) && self.job_pgid.is_none() {
                            let _ = setpgid(child, child);
                            crate::agent::note_foreground(child.as_raw());
                        }
                        self.set_foreground(child);
                        let pgid = self.job_pgid.unwrap_or(child);
                        self.wait_foreground(&[child], pgid, program.clone())
                    }
                    Ok(ForkResult::Child) => {
                        if self.interactive || self.isolate_groups {
                            let target = self.job_pgid.unwrap_or(Pid::from_raw(0));
                            let _ = setpgid(Pid::from_raw(0), target);
                        }
                        crate::agent::child_guard();
                        self.reset_child_traps();
                        crate::signal::restore_mask(mask);
                        exec::exec_replace(&path, argv, &env)
                    }
                    Err(err) => {
                        crate::signal::restore_mask(mask);
                        let line = format!("sharky: fork: {err}\n");
                        let _ = crate::term::write_stderr(&line);
                        126
                    }
                }
            }
        };
        self.last_status = status;
        self.set_pipe_status(&[status]);
        let elapsed = started.elapsed();
        self.last_duration_ms = elapsed.as_millis() as u64;
        self.last_duration_us = elapsed.as_micros() as u64;
        status
    }

    #[allow(dead_code)] // read by the session-surface unit tests only
    pub(crate) fn isolate_groups(&self) -> bool {
        self.isolate_groups
    }

    pub(crate) fn set_isolate_groups(&mut self, on: bool) {
        self.isolate_groups = on;
    }

    #[allow(dead_code)] // read by the session-surface unit tests only
    pub(crate) fn session_mode(&self) -> bool {
        self.session_mode
    }

    pub(crate) fn set_session_mode(&mut self, on: bool) {
        self.session_mode = on;
    }

    /// Applies one `tasks[].env` entry to the task's shell copy (design §2.1).
    ///
    /// The override is an *external* context, the way `env NAME=value command`
    /// is: it is handed to the task by its parent, not assigned by the shell,
    /// so the shell's `readonly` attribute never decides it for the task copy.
    /// `Some(value)` sets the variable, gives it the export attribute (so
    /// `$NAME` and a child process agree) and drops an inherited readonly
    /// attribute, because `env` hands the command a plain variable; `None`
    /// removes the value along with the export and readonly attributes, so a
    /// readonly session variable can still be hidden from one task. Only ever
    /// called in a forked child: the session's own environment and its
    /// readonly set cannot change, and global readonly semantics elsewhere are
    /// untouched.
    pub(crate) fn set_task_env(&mut self, name: &str, value: Option<String>) {
        match value {
            Some(value) => {
                self.set_var_unchecked(name, value);
                self.mark_exported(name);
                self.readonly_vars.remove(name);
            }
            None => {
                self.env.retain(|(key, _)| key != name);
                self.exported.remove(name);
                self.readonly_vars.remove(name);
            }
        }
    }

    /// Applies `tasks[].cwd` to the task's shell copy: it changes the process
    /// directory (so commands and their children see it) and updates
    /// `self.cwd`, `PWD` and `OLDPWD` the way the `cd` builtin does, so `pwd`
    /// and `$PWD` agree with the real directory. Only ever called in a forked
    /// child.
    pub(crate) fn enter_task_directory(&mut self, path: &str) -> std::io::Result<()> {
        let previous = self.cwd.clone();
        std::env::set_current_dir(path)?;
        self.cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
        self.set_var("OLDPWD", previous.display().to_string());
        self.set_var("PWD", self.cwd.display().to_string());
        Ok(())
    }

    /// Prepares the forked child of a parallel task before it evaluates its
    /// command. The child owns a copy-on-write copy of the session's `Shell`,
    /// so nothing written here can reach the parent: this only puts the copy
    /// in the state a task needs.
    ///
    /// Called after `agent::child_guard` (which turns the session off in the
    /// child) and after the task's output has been moved onto 1 and 2.
    #[allow(dead_code)] // called by the parallel driver and the unit tests
    pub(crate) fn enter_task_child(&mut self) {
        // A task is a subshell of the session's interpreter state that never
        // drives the protocol and never touches the terminal.
        self.interactive = false;
        self.in_subshell = true;
        self.session_mode = true;
        self.isolate_groups = true;

        // The task leads its own process group, so every child it starts joins
        // that group instead of leading one of its own: `job_pgid` gates the
        // `setpgid` calls in `spawn`, `run_subshell` and `run_background`, and
        // one signal then reaches the whole task. The driver applies the same
        // call in the parent, closing the race the other fork sites close.
        let pid = getpid();
        if getpgrp() != pid {
            let _ = setpgid(Pid::from_raw(0), pid);
        }
        self.job_pgid = Some(pid);

        // The foreground group of a request belongs to the parent session:
        // `child_guard` already left the supervisor inactive in this child,
        // and this keeps the entry point correct for a child that still has it
        // active. A cancellation aimed at the session must not pre-exit the
        // task either.
        crate::agent::clear_foreground();
        self.exit_requested = None;

        // A task is a fresh command string, not the continuation of the
        // session's input, so an inherited `set -e` must not abort the task
        // body: this is the `-c` contract, and the same reason command
        // substitution turns errexit off in its child. A task that asks for
        // errexit itself (`set -e` inside the command) still gets it.
        self.errexit = false;
        self.errexit_suppressed = false;

        // Caught signals go back to their default dispositions in the child,
        // but the EXIT action is the task's to run: snapshot it, reset, put it
        // back. `finish_task` then runs it exactly once at task end, and the
        // driver skips it when the task never set an action of its own, so a
        // session-level `trap ... EXIT` does not fire once per task. The flag
        // starts false: this shell has not defined an action yet, only
        // inherited one.
        let exit = self.snapshot_exit_trap();
        self.reset_child_traps();
        self.restore_exit_trap(exit);
        self.exit_trap_ran = false;
        self.exit_trap_defined_here = false;
    }

    /// The EXIT action a forked child inherited, taken before
    /// [`Shell::enter_task_child`] resets the child's traps. The parallel
    /// driver compares it with the action after the task's command ran: an
    /// unchanged action is the session's and must not fire once per task,
    /// while a task that set one of its own gets it run by `finish`.
    pub(crate) fn snapshot_exit_trap(&self) -> Option<String> {
        self.traps.get("EXIT").cloned()
    }

    /// Puts a snapshotted EXIT action back after a child trap reset.
    pub(crate) fn restore_exit_trap(&mut self, action: Option<String>) {
        match action {
            Some(action) => {
                self.traps.insert("EXIT".to_string(), action);
            }
            None => {
                self.traps.remove("EXIT");
            }
        }
    }

    /// Every stored variable as `(name, value)`, sorted by name — the same set
    /// and order the `state --json --env` path reports.
    pub(crate) fn snapshot_env(&self) -> Vec<(String, String)> {
        let mut env = self.env.clone();
        env.sort_by(|left, right| left.0.cmp(&right.0));
        env
    }

    /// Scalar bindings as `(name, value, exported, readonly)`, sorted by name.
    /// A name carrying no value (`export FOO`) reports `None`. Names that are
    /// arrays or associative arrays are left to [`Shell::snapshot_arrays`] and
    /// [`Shell::snapshot_assoc`], so one binding is never reported twice.
    pub(crate) fn snapshot_variables(&self) -> Vec<(String, Option<String>, bool, bool)> {
        let mut names: Vec<&String> = self.env.iter().map(|(name, _)| name).collect();
        names.extend(self.exported.iter());
        names.extend(self.readonly_vars.iter());
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter(|name| !self.arrays.contains_key(*name) && !self.assoc.contains_key(*name))
            .map(|name| {
                (
                    name.clone(),
                    self.env
                        .iter()
                        .find(|(key, _)| key == name)
                        .map(|(_, value)| value.clone()),
                    self.exported.contains(name),
                    self.readonly_vars.contains(name),
                )
            })
            .collect()
    }

    /// Indexed arrays by name. An unset element stays `None`, so an unset hole
    /// is never confused with the string `"null"`.
    pub(crate) fn snapshot_arrays(&self) -> BTreeMap<String, Vec<Option<String>>> {
        self.arrays
            .iter()
            .map(|(name, values)| (name.clone(), values.clone()))
            .collect()
    }

    /// Associative arrays by name, with each key order kept as the shell keeps
    /// it (`BTreeMap`, keyed lexicographically).
    pub(crate) fn snapshot_assoc(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        self.assoc
            .iter()
            .map(|(name, map)| (name.clone(), map.clone()))
            .collect()
    }

    pub(crate) fn take_snapshot(&self) -> ShellSnapshot {
        ShellSnapshot {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            exported: self.exported.clone(),
            readonly_vars: self.readonly_vars.clone(),
            arrays: self.arrays.clone(),
            assoc: self.assoc.clone(),
            namerefs: self.namerefs.clone(),
            int_vars: self.int_vars.clone(),
            lower_vars: self.lower_vars.clone(),
            upper_vars: self.upper_vars.clone(),
            funcs: self.funcs.clone(),
            traps: self.traps.clone(),
            positional: self.positional.clone(),
            aliases: self.aliases.clone(),
            abbreviations: self.abbreviations.clone(),
            errexit: self.errexit,
            nounset: self.nounset,
            xtrace: self.xtrace,
            pipefail: self.pipefail,
            noclobber: self.noclobber,
            errtrace: self.errtrace,
            functrace: self.functrace,
            globstar: self.globstar,
            nullglob: self.nullglob,
            dotglob: self.dotglob,
            failglob: self.failglob,
            nocasematch: self.nocasematch,
            extglob: self.extglob,
        }
    }

    pub(crate) fn restore_snapshot(&mut self, snap: &ShellSnapshot) {
        self.cwd = snap.cwd.clone();
        let _ = std::env::set_current_dir(&self.cwd);
        self.env = snap.env.clone();
        self.exported = snap.exported.clone();
        self.readonly_vars = snap.readonly_vars.clone();
        self.arrays = snap.arrays.clone();
        self.assoc = snap.assoc.clone();
        self.namerefs = snap.namerefs.clone();
        self.int_vars = snap.int_vars.clone();
        self.lower_vars = snap.lower_vars.clone();
        self.upper_vars = snap.upper_vars.clone();
        self.funcs = snap.funcs.clone();
        self.traps = snap.traps.clone();
        self.positional = snap.positional.clone();
        self.aliases = snap.aliases.clone();
        self.abbreviations = snap.abbreviations.clone();
        self.errexit = snap.errexit;
        self.nounset = snap.nounset;
        self.xtrace = snap.xtrace;
        self.pipefail = snap.pipefail;
        self.noclobber = snap.noclobber;
        self.errtrace = snap.errtrace;
        self.functrace = snap.functrace;
        self.globstar = snap.globstar;
        self.nullglob = snap.nullglob;
        self.dotglob = snap.dotglob;
        self.failglob = snap.failglob;
        self.nocasematch = snap.nocasematch;
        self.extglob = snap.extglob;
        self.command_cache.clear();
        self.highlight_cache.borrow_mut().clear();
    }

    /// The job table as `(id, pgid, pids, command, stopped)`, in the order the
    /// shell keeps it. Read-only: `state` formats it, nothing here changes a
    /// job or reaps one.
    pub(crate) fn snapshot_jobs(&self) -> Vec<(usize, i32, Vec<i32>, String, bool)> {
        self.jobs
            .iter()
            .map(|job| {
                (
                    job.id,
                    job.pgid.as_raw(),
                    job.pids.iter().map(|pid| pid.as_raw()).collect(),
                    job.command.clone(),
                    job.stop.is_some(),
                )
            })
            .collect()
    }

    pub(crate) fn snapshot_functions(&self) -> Vec<String> {
        let mut names: Vec<String> = self.funcs.keys().cloned().collect();
        names.sort();
        names
    }

    pub(crate) fn snapshot_aliases(&self) -> Vec<(String, String)> {
        let mut aliases: Vec<(String, String)> = self
            .aliases
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        aliases.sort();
        aliases
    }

    pub(crate) fn set_alias(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.aliases.insert(name.into(), value.into());
    }

    pub(crate) fn snapshot_traps(&self) -> Vec<(String, String)> {
        let mut traps: Vec<(String, String)> = self
            .traps
            .iter()
            .map(|(signal, action)| (signal.clone(), action.clone()))
            .collect();
        traps.sort();
        traps
    }

    /// The shell options a session reports, in a fixed order.
    pub(crate) fn snapshot_options(&self) -> Vec<(&'static str, bool)> {
        vec![
            ("errexit", self.errexit),
            ("nounset", self.nounset),
            ("pipefail", self.pipefail),
            ("noclobber", self.noclobber),
            ("xtrace", self.xtrace),
            ("histexpand", self.histexpand),
        ]
    }

    /// The variables that changed between `before` and now, sorted by name:
    /// `Some(value)` for a new or changed value, `None` for a removed one.
    /// `_` is excluded because it changes after every command by design. This
    /// is the one implementation of `env_delta` semantics; the `--json -c`
    /// path formats the result it returns.
    pub(crate) fn env_delta(&self, before: &[(String, String)]) -> Vec<(String, Option<String>)> {
        let mut delta: Vec<(String, Option<String>)> = Vec::new();
        for (key, value) in &self.env {
            if key == "_" {
                continue;
            }
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
            if key == "_" {
                continue;
            }
            if !self.env.iter().any(|(name, _)| name == key) {
                delta.push((key.clone(), None));
            }
        }
        delta.sort_by(|left, right| left.0.cmp(&right.0));
        delta
    }
}

impl Shell {
    pub fn keybinds(&self) -> &HashMap<String, String> {
        &self.keybinds
    }

    pub fn abbreviation(&self, name: &str) -> Option<&str> {
        self.abbreviations.get(name).map(String::as_str)
    }

    pub fn wake_commands(&self) -> &[String] {
        &self.wake
    }

    pub fn jobs_count(&self) -> usize {
        self.jobs.len()
    }

    pub fn history_limit(&self) -> usize {
        self.settings
            .get("history")
            .and_then(|value| value.parse().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(1000)
    }

    pub fn style(&self, key: &str) -> Option<&str> {
        self.styles.get(key).map(String::as_str)
    }

    pub fn colour(&self, name: &str) -> Option<&str> {
        self.colors.get(name).map(String::as_str)
    }

    pub fn colour_enabled(&self) -> bool {
        !self.styles.is_empty() && std::env::var_os("NO_COLOR").is_none()
    }

    pub fn autocd_enabled(&self) -> bool {
        if let Some(flag) = self.autocd {
            return flag;
        }
        match self.setting("autocd") {
            Some("on") => true,
            Some("off") => false,
            _ => self.interactive,
        }
    }

    pub fn classify_command_word(&self, word: &str) -> crate::highlight::Role {
        use crate::highlight::Role;
        use std::os::unix::fs::PermissionsExt;

        if is_builtin_name(word)
            || matches!(
                word,
                "if" | "then"
                    | "elif"
                    | "else"
                    | "fi"
                    | "for"
                    | "in"
                    | "while"
                    | "until"
                    | "do"
                    | "done"
                    | "case"
                    | "esac"
                    | "function"
                    | "time"
                    | "{"
                    | "}"
                    | "!"
                    | "[["
                    | "]]"
            )
            || self.funcs.contains_key(word)
            || self.aliases.contains_key(word)
            || self.abbreviations.contains_key(word)
        {
            return Role::Builtin;
        }

        if let Some((name, _)) = word.split_once('=')
            && !name.is_empty()
            && name
                .chars()
                .enumerate()
                .all(|(i, c)| if i == 0 { c.is_ascii_alphabetic() || c == '_' } else { c.is_ascii_alphanumeric() || c == '_' })
        {
            return Role::Variable;
        }

        if word.contains('/') || word.starts_with('~') || word == "." || word == ".." {
            let expanded = if word == "~" {
                self.var("HOME")
            } else if let Some(rest) = word.strip_prefix("~/") {
                format!("{}/{rest}", self.var("HOME").trim_end_matches('/'))
            } else {
                word.to_string()
            };
            let path = Path::new(&expanded);
            let full = if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.cwd.join(path)
            };
            if let Ok(meta) = std::fs::metadata(&full) {
                if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                    return Role::Command;
                }
                if self.autocd_enabled() && meta.is_dir() {
                    return Role::Command;
                }
            }
            return Role::Error;
        }

        let cached = self.highlight_cache.borrow().get(word).copied();
        let found = match cached {
            Some(hit) => hit,
            None => {
                let hit = exec::hashed_path(&self.command_cache, word).is_some()
                    || exec::lookup(word, self)
                        .is_some_and(|path| exec::is_executable(&path));
                let mut cache = self.highlight_cache.borrow_mut();
                if cache.len() > 2048 {
                    cache.clear();
                }
                cache.insert(word.to_string(), hit);
                hit
            }
        };
        if found {
            return Role::Command;
        }
        if self.autocd_enabled() && self.cwd.join(word).is_dir() {
            return Role::Command;
        }
        if self.path_dirs().is_empty() {
            return Role::Command;
        }
        Role::Error
    }

    pub fn command_words(&self) -> Vec<String> {
        let mut words: Vec<String> = [
            ":", "true", "false", "cd", "pushd", "popd", "dirs", "ulimit", "hash",
            "let", "times",
            "exit", "export", "unset", "set", "source", ".",
            "break", "continue", "return", "jobs", "fg", "bg", "disown", "declare", "local", "pwd", "type",
            "alias", "abbr", "unabbr", "unalias", "history", "wait", "kill", "z", "zi",
            "reload", "trap", "read", "shift", "wake", "exec", "time", "shopt",
            "mapfile", "readarray", "command", "help", "echo", "printf",
            "test", "[", "eval", "umask", "getopts",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        words.extend(self.aliases.keys().cloned());
        words.extend(self.abbreviations.keys().cloned());
        words.extend(self.funcs.keys().cloned());
        words
    }

    pub fn path_dirs(&self) -> Vec<String> {
        self.var("PATH")
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn suggest_similar_command(&self, typo: &str) -> Option<String> {
        if typo.is_empty() || typo.contains('/') || typo == "." || typo == ".." {
            return None;
        }
        let typo_chars: Vec<char> = typo.chars().collect();
        if typo_chars.len() <= 1 {
            return None;
        }
        let max_dist = 2usize.min(typo_chars.len().saturating_sub(1));
        let mut best: Option<(usize, String)> = None;
        let mut consider = |candidate: &str| {
            if candidate == typo || candidate.is_empty() {
                return;
            }
            let cand_chars: Vec<char> = candidate.chars().collect();
            if typo_chars.len().abs_diff(cand_chars.len()) > max_dist {
                return;
            }
            let dist = Self::levenshtein(&typo_chars, &cand_chars);
            if dist == 0 || dist > max_dist || dist >= typo_chars.len() {
                return;
            }
            let better = match &best {
                None => true,
                Some((best_dist, best_name)) => {
                    dist < *best_dist || (dist == *best_dist && candidate < best_name.as_str())
                }
            };
            if better {
                best = Some((dist, candidate.to_string()));
            }
        };

        for word in self.command_words() {
            consider(&word);
        }

        let mut seen_dirs = HashSet::new();
        for dir in self.path_dirs() {
            if !seen_dirs.insert(dir.clone()) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if typo_chars.len().abs_diff(name.chars().count()) > max_dist {
                    continue;
                }
                if exec::is_executable(&entry.path()) {
                    consider(name);
                }
            }
        }

        best.map(|(_, name)| name)
    }

    fn levenshtein(a: &[char], b: &[char]) -> usize {
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut curr = vec![0usize; b.len() + 1];
        for (i, &ca) in a.iter().enumerate() {
            curr[0] = i + 1;
            for (j, &cb) in b.iter().enumerate() {
                let cost = usize::from(ca != cb);
                curr[j + 1] = (prev[j + 1] + 1)
                    .min(curr[j] + 1)
                    .min(prev[j] + cost);
            }
            std::mem::swap(&mut prev, &mut curr);
        }
        prev[b.len()]
    }

    fn foreground_control(&self) -> bool {
        self.interactive && self.job_pgid.is_none()
    }

    // Hands the terminal to `pgid`. The descriptor is opened per call: a stored
    // number is a redirect target like any other, so `exec 3> file` would take
    // the shell's own terminal away for good.
    fn set_foreground(&self, pgid: Pid) {
        if !self.foreground_control() {
            return;
        }

        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok();

        // SAFETY: fd 0 is only borrowed while it is an open terminal.
        let tty = match &opened {
            Some(file) => file.as_fd(),
            None if unsafe { nix::libc::isatty(0) } == 1 => unsafe {
                std::os::fd::BorrowedFd::borrow_raw(0)
            },
            None => return,
        };

        let _ = tcsetpgrp(tty, pgid);
    }

    fn reclaim_terminal(&self) {
        self.set_foreground(getpgrp());
    }

    fn run_program(&mut self, program: &Program) -> Flow {
        let mut status = self.last_status;

        for line in &program.lines {
            match self.run_list(line) {
                Flow::Status(code) => {
                    status = code;
                    self.last_status = code;
                }
                // A fatal error ends the line that produced it, not the input:
                // the shell keeps its state and runs the next line, which sees
                // the failed one's status.
                Flow::Discard(code) => {
                    status = code;
                    self.last_status = code;
                }
                other => return other,
            }
        }

        Flow::Status(status)
    }

    fn run_list(&mut self, list: &List) -> Flow {
        let mut status = self.last_status;

        for entry in &list.entries {
            if let Some(code) = self.service_traps() {
                return Flow::Exit(code);
            }
            if let Some(signal) = crate::signal::fatal_signal() {
                return Flow::Exit(128 + signal);
            }
            if crate::signal::take_interrupt() {
                return Flow::Interrupt;
            }

            if entry.line > 0 && !self.trap_running {
                self.current_line = entry.line;
            }
            if entry.background {
                self.run_background(&entry.and_or);
                status = 0;
                self.last_status = 0;
                continue;
            }

            match self.run_and_or(&entry.and_or) {
                Flow::Status(code) => {
                    status = code;
                    self.last_status = code;
                }
                other => return other,
            }

            // An action that asked to exit while the shell waited for the
            // command has no flow value of its own yet.
            if let Some(code) = self.service_traps() {
                return Flow::Exit(code);
            }
            if let Some(signal) = crate::signal::fatal_signal() {
                return Flow::Exit(128 + signal);
            }
            if crate::signal::take_interrupt() {
                return Flow::Interrupt;
            }
        }

        if let Some(code) = self.service_traps() {
            return Flow::Exit(code);
        }
        if let Some(signal) = crate::signal::fatal_signal() {
            return Flow::Exit(128 + signal);
        }
        if crate::signal::take_interrupt() {
            return Flow::Interrupt;
        }

        Flow::Status(status)
    }

    fn run_background(&mut self, and_or: &AndOr) {
        // The mask is held from before the fork until the child restored its
        // own dispositions: a SIGTERM that arrives in between stays pending and
        // then takes its default action instead of reaching a shell handler
        // that would swallow it.
        let mask = crate::signal::block_all();
        // SAFETY: the child runs shell code and exits without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                // A background job leads a new group only at top level. When
                // the shell already leads one (a task child, or a background
                // job nesting another), the child stays in that group so a
                // single group signal reaches the whole subtree. The child
                // inherits the pgid across the fork; the call closes the race.
                let group = self.job_pgid.unwrap_or(child);
                let _ = setpgid(child, group);
                if self.job_pgid.is_none() {
                    crate::agent::note_group(child.as_raw());
                }
                let id = self.next_job;
                self.next_job += 1;
                self.last_bg_pid = Some(child);
                // Like the Done/Stopped notices, the job id line belongs to
                // interactive use: in a script or a command substitution it
                // would land in the output bash keeps clean.
                if self.interactive {
                    println!("[{id}] {child}");
                }
                self.jobs.push(Job {
                    id,
                    pgid: group,
                    pids: vec![child],
                    command: describe_and_or(and_or),
                    stop: None,
                    no_hup: false,
                });
                self.publish_jobs();
            }
            Ok(ForkResult::Child) => {
                let group = self.job_pgid.unwrap_or_else(getpid);
                let _ = setpgid(Pid::from_raw(0), group);
                crate::agent::child_guard();
                self.reset_child_traps();
                crate::signal::restore_mask(mask);
                self.job_pgid = Some(group);
                let code = flow_status(self.run_and_or(and_or));
                unsafe { _exit(code) }
            }
            Err(err) => {
                crate::signal::restore_mask(mask);
                eprintln!("sharky: fork: {err}");
            }
        }
    }

    fn run_coproc(&mut self, name: &str, command: &Command) -> Flow {
        let Ok((p2c_r, p2c_w)) = pipe2(OFlag::empty()) else {
            eprintln!("sharky: coproc: pipe failed");
            return Flow::Status(1);
        };
        let Ok((c2p_r, c2p_w)) = pipe2(OFlag::empty()) else {
            eprintln!("sharky: coproc: pipe failed");
            return Flow::Status(1);
        };

        let mask = crate::signal::block_all();
        // SAFETY: the child runs shell code and exits without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                let group = self.job_pgid.unwrap_or(child);
                let _ = setpgid(child, group);
                if self.job_pgid.is_none() {
                    crate::agent::note_group(child.as_raw());
                }
                drop(p2c_r);
                drop(c2p_w);
                let read_fd = c2p_r.into_raw_fd();
                let write_fd = p2c_w.into_raw_fd();
                self.set_array(name, vec![read_fd.to_string(), write_fd.to_string()], false);
                self.set_var(&format!("{name}_PID"), child.to_string());
                self.last_bg_pid = Some(child);
                let id = self.next_job;
                self.next_job += 1;
                if self.interactive {
                    println!("[{id}] {child}");
                }
                self.jobs.push(Job {
                    id,
                    pgid: group,
                    pids: vec![child],
                    command: describe_command(command),
                    stop: None,
                    no_hup: false,
                });
                self.publish_jobs();
                Flow::Status(0)
            }
            Ok(ForkResult::Child) => {
                let group = self.job_pgid.unwrap_or_else(getpid);
                let _ = setpgid(Pid::from_raw(0), group);
                crate::agent::child_guard();
                self.reset_child_traps();
                crate::signal::restore_mask(mask);
                self.job_pgid = Some(group);
                drop(p2c_w);
                drop(c2p_r);
                let r_fd = p2c_r.into_raw_fd();
                let w_fd = c2p_w.into_raw_fd();
                if r_fd != 0 {
                    let _ = move_fd(r_fd, 0);
                    if r_fd != 1 {
                        let _ = close(r_fd);
                    }
                }
                if w_fd != 1 {
                    let _ = move_fd(w_fd, 1);
                    if w_fd != 0 {
                        let _ = close(w_fd);
                    }
                }
                let code = self.run_command_child(command, false);
                unsafe { _exit(code) }
            }
            Err(err) => {
                crate::signal::restore_mask(mask);
                eprintln!("sharky: fork: {err}");
                Flow::Status(1)
            }
        }
    }

    fn run_and_or(&mut self, and_or: &AndOr) -> Flow {
        let previous = self.errexit_suppressed;
        if !and_or.rest.is_empty() {
            self.errexit_suppressed = true;
        }
        let mut flow = self.run_pipeline(&and_or.first);

        for (index, (is_and, pipeline)) in and_or.rest.iter().enumerate() {
            if let Some(code) = self.service_traps() {
                self.errexit_suppressed = previous;
                return Flow::Exit(code);
            }
            if let Some(signal) = crate::signal::fatal_signal() {
                self.errexit_suppressed = previous;
                return Flow::Exit(128 + signal);
            }
            if crate::signal::take_interrupt() {
                self.errexit_suppressed = previous;
                return Flow::Interrupt;
            }
            let status = match &flow {
                Flow::Status(code) => *code,
                _ => {
                    self.errexit_suppressed = previous;
                    return flow;
                }
            };
            let should_run = if *is_and { status == 0 } else { status != 0 };
            if should_run {
                self.last_status = status;
                self.errexit_suppressed = previous || index + 1 != and_or.rest.len();
                flow = self.run_pipeline(pipeline);
            }
        }

        self.errexit_suppressed = previous;
        flow
    }

    fn run_suppressed(&mut self, list: &List) -> Flow {
        let previous = self.errexit_suppressed;
        self.errexit_suppressed = true;
        let flow = self.run_list(list);
        self.errexit_suppressed = previous;
        flow
    }

    fn errexit_check(&mut self, flow: Flow) -> Flow {
        if self.nounset_failed {
            return self.unset_parameter_failure();
        }
        match flow {
            Flow::Status(code)
                if code != 0 && self.errexit && !self.errexit_suppressed =>
            {
                if self.interactive {
                    return Flow::Status(code);
                }
                self.exit_requested = Some(code);
                Flow::Exit(code)
            }
            other => other,
        }
    }

    fn run_pipeline(&mut self, pipeline: &Pipeline) -> Flow {
        let previous = self.errexit_suppressed;
        if pipeline.negate {
            self.errexit_suppressed = true;
        }
        let flow = self.run_pipeline_measured(pipeline);
        self.errexit_suppressed = previous;
        negate_flow(flow, pipeline.negate)
    }

    fn run_pipeline_measured(&mut self, pipeline: &Pipeline) -> Flow {
        let Some(posix) = pipeline.timed else {
            return self.run_pipeline_inner(pipeline);
        };

        let started = std::time::Instant::now();
        let (self_user, self_sys) = cpu_usage(nix::libc::RUSAGE_SELF);
        let (child_user, child_sys) = cpu_usage(nix::libc::RUSAGE_CHILDREN);
        let flow = self.run_pipeline_inner(pipeline);
        let real = started.elapsed().as_secs_f64();
        let (self_user_after, self_sys_after) = cpu_usage(nix::libc::RUSAGE_SELF);
        let (child_user_after, child_sys_after) = cpu_usage(nix::libc::RUSAGE_CHILDREN);
        let user = (self_user_after - self_user).max(0.0) + (child_user_after - child_user).max(0.0);
        let sys = (self_sys_after - self_sys).max(0.0) + (child_sys_after - child_sys).max(0.0);
        report_time(real, user, sys, posix);
        flow
    }

    fn run_pipeline_inner(&mut self, pipeline: &Pipeline) -> Flow {
        if pipeline.commands.len() == 1 {
            if matches!(pipeline.commands[0], Command::Not(_)) {
                let previous = self.errexit_suppressed;
                self.errexit_suppressed = true;
                let flow = self.run_command(&pipeline.commands[0]);
                self.errexit_suppressed = previous;
                self.reap_substitutions();
                return flow;
            }
            let flow = self.run_command(&pipeline.commands[0]);
            self.maybe_fire_err_trap(Some(&pipeline.commands[0]), &flow);
            let flow = self.errexit_check(flow);
            self.reap_substitutions();
            return flow;
        }

        let links = pipeline.commands.len() - 1;
        let mut pipes = Vec::with_capacity(links);
        for _ in 0..links {
            match pipe2(OFlag::O_CLOEXEC) {
                Ok(pair) => pipes.push(pair),
                Err(err) => {
                    eprintln!("sharky: pipe: {err}");
                    return Flow::Status(1);
                }
            }
        }

        let mut pids = Vec::with_capacity(pipeline.commands.len());
        let mut leader: Option<Pid> = None;
        let pipe_fds: Vec<RawFd> = pipes
            .iter()
            .flat_map(|pair| [pair.0.as_raw_fd(), pair.1.as_raw_fd()])
            .collect();
        for (index, command) in pipeline.commands.iter().enumerate() {
            let stdin = index.checked_sub(1).map(|i| pipes[i].0.as_raw_fd());
            let stdout = pipes.get(index).map(|pair| pair.1.as_raw_fd());

            // The mask around fork keeps a signal aimed at the fresh child from
            // reaching an inherited shell handler before the child restored the
            // default dispositions.
            let mask = crate::signal::block_all();
            // SAFETY: the child runs shell code and exits without unwinding.
            match unsafe { fork() } {
                Ok(ForkResult::Parent { child }) => {
                    crate::signal::restore_mask(mask);
                    if self.interactive || self.isolate_groups {
                        match self.job_pgid.or(leader) {
                            // A pipeline inside a subshell belongs to the
                            // subshell's group, so the whole subshell stays one
                            // job of the outer shell.
                            Some(group) => {
                                let _ = setpgid(child, group);
                            }
                            None => {
                                // The first member of a foreground pipeline
                                // leads the job. Setting the group here as well
                                // as in the child closes the ESRCH race with
                                // the terminal hand-off.
                                let _ = setpgid(child, child);
                                crate::agent::note_foreground(child.as_raw());
                                self.set_foreground(child);
                            }
                        }
                        if leader.is_none() {
                            leader = Some(child);
                        }
                    }
                    pids.push(child);
                }
                Ok(ForkResult::Child) => {
                    if self.interactive || self.isolate_groups {
                        let target = self.job_pgid.or(leader).unwrap_or(Pid::from_raw(0));
                        let _ = setpgid(Pid::from_raw(0), target);
                    }
                    crate::agent::child_guard();
                    self.reset_child_traps();
                    crate::signal::restore_mask(mask);
                    self.in_pipeline_member = true;
                    if let Some(fd) = stdin
                        && move_fd(fd, 0).is_err()
                    {
                        unsafe { _exit(126) }
                    }
                    if let Some(fd) = stdout
                        && move_fd(fd, 1).is_err()
                    {
                        unsafe { _exit(126) }
                    }
                    // Shell code in this child never execs, so O_CLOEXEC never
                    // fires: close every pipeline end except the ones dup'd
                    // onto 0 and 1, or the child keeps its own pipe open and
                    // `read` never sees EOF. Matching on the source fd number
                    // keeps this exact if pipe2 ever hands out 0 or 1.
                    for fd in &pipe_fds {
                        let kept = (*fd == 0 && stdin == Some(*fd))
                            || (*fd == 1 && stdout == Some(*fd));
                        if !kept {
                            let _ = close(*fd);
                        }
                    }
                    let code = self.run_command_child(
                        command,
                        pipeline.merge_stderr.get(index).copied().unwrap_or(false),
                    );
                    unsafe { _exit(code) }
                }
                Err(err) => {
                    crate::signal::restore_mask(mask);
                    eprintln!("sharky: fork: {err}");
                    return Flow::Status(1);
                }
            }
        }

        drop(pipes);

        let command = describe_pipeline(pipeline);
        let pgid = self
            .job_pgid
            .unwrap_or_else(|| leader.unwrap_or_else(getpgrp));
        let status = self.wait_pipeline(&pids, pgid, command);
        self.reap_substitutions();
        let flow = Flow::Status(status);
        self.maybe_fire_err_trap(None, &flow);
        self.errexit_check(flow)
    }

    fn run_command(&mut self, command: &Command) -> Flow {
        let flow = match command {
            Command::Simple(simple) => self.run_simple(simple),
            // `!` inverts `$?` only: `PIPESTATUS` keeps the command's own
            // status, so the array is published before the inversion.
            Command::Not(inner) => {
                let flow = self.run_command(inner);
                return match flow {
                    Flow::Status(code) => {
                        if publishes_pipe_status(inner) {
                            self.set_pipe_status(&[code]);
                        }
                        Flow::Status(if code == 0 { 1 } else { 0 })
                    }
                    other => other,
                };
            }
            Command::Group(list) => self.run_list(list),
            Command::Subshell(list) => self.run_subshell(list),
            Command::If {
                branches,
                else_body,
            } => self.run_if(branches, else_body.as_ref()),
            Command::For { name, words, body } => self.run_for(name, words, body),
            Command::ForArith {
                init,
                condition,
                step,
                body,
            } => self.run_for_arith(init, condition, step, body),
            Command::While {
                condition,
                body,
                until,
            } => self.run_while(condition, body, *until),
            Command::Case { word, arms } => self.run_case(word, arms),
            Command::Function { name, body, line } => {
                let _ = line;
                let source_file = self
                    .bash_source
                    .first()
                    .cloned()
                    .unwrap_or_else(|| self.script.clone().unwrap_or_else(|| "sharky".to_string()));
                self.func_sources.insert(name.clone(), source_file);
                self.funcs.insert(name.clone(), body.clone());
                Flow::Status(0)
            }
            Command::Redirected {
                command,
                redirects,
            } => self.run_redirected(command, redirects, false),
            Command::Coproc { name, command } => self.run_coproc(name, command),
            Command::Test(words) => self.run_test(words),
            Command::Arithmetic(source) => Flow::Status(match arith::evaluate(source, self) {
                Ok(0) => 1,
                Ok(_) => 0,
                Err(error) => arithmetic_failure(error),
            }),
        };

        // Bash records an array for each pipeline and each simple command;
        // compound commands that run in the shell itself keep the array their
        // last inner command left behind.
        if publishes_pipe_status(command)
            && let Flow::Status(code) = flow
        {
            self.set_pipe_status(&[code]);
        }

        flow
    }

    fn run_test(&mut self, words: &[Word]) -> Flow {
        let result = test_expr::evaluate(words, self);
        if let Some(flow) = self.take_expansion_failure() {
            return flow;
        }
        Flow::Status(match result {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(message) => {
                eprintln!("sharky: [[: {message}");
                2
            }
        })
    }

    fn maybe_fire_err_trap(&mut self, command: Option<&Command>, flow: &Flow) {
        let Flow::Status(code) = *flow else {
            return;
        };
        if code == 0 || self.errexit_suppressed || self.trap_running {
            return;
        }
        if !self.errtrace && !self.funcname.is_empty() {
            return;
        }
        if matches!(
            command,
            Some(
                Command::Group(_)
                    | Command::If { .. }
                    | Command::For { .. }
                    | Command::ForArith { .. }
                    | Command::While { .. }
                    | Command::Case { .. }
            )
        ) {
            return;
        }
        if let Some(action) = self.traps.get("ERR").cloned()
            && !action.trim().is_empty()
        {
            self.last_status = code;
            self.run_trap_action(&action);
        }
    }

    fn maybe_fire_return_trap(&mut self, status: i32) {
        if self.trap_running || !self.functrace {
            return;
        }
        if let Some(action) = self.traps.get("RETURN").cloned()
            && !action.trim().is_empty()
        {
            self.last_status = status;
            self.run_trap_action(&action);
        }
    }

    fn run_simple(&mut self, simple: &Simple) -> Flow {
        if simple.line > 0 && !self.trap_running {
            self.current_line = simple.line;
        }
        let substitutions = self.substitution_fds.len();
        let flow = self.run_simple_inner(simple);
        self.close_substitutions_from(substitutions);
        flow
    }

    fn run_simple_inner(&mut self, simple: &Simple) -> Flow {
        if simple.words.is_empty() {
            return self.run_assignments(simple);
        }

        let (flow, argument) = self.run_simple_dispatch(simple);
        if let Some(argument) = argument {
            self.remember_underscore(argument);
        }
        flow
    }

    // Runs one simple command and reports the last expanded argument, which
    // becomes `$_` once the command finishes. Bash updates `_` after the
    // command, so `read _` and `printf -v _` still see their own value
    // overwritten by the command's last word.
    fn run_simple_dispatch(&mut self, simple: &Simple) -> (Flow, Option<String>) {
        let mut prepared = match self.prepare(simple) {
            Ok(prepared) => prepared,
            Err(message) => {
                if !message.is_empty() {
                    eprintln!("sharky: {message}");
                }
                return (self.expansion_failure(), None);
            }
        };

        if self.nounset_failed {
            return (self.unset_parameter_failure(), None);
        }

        if let Some(name) = prepared.argv.first()
            && let Some(expansion) = self.aliases.get(name).cloned()
            && let Some(argv) = self.expand_alias(&expansion, &prepared.argv[1..])
        {
            prepared.argv = argv;
        }

        // `exec` applies its own adjustment, which differs inside a subshell.
        if self.shlvl_tail == Some(simple as *const Simple)
            && prepared.argv.first().map(String::as_str) != Some("exec")
        {
            self.ensure_child_env(&mut prepared);
            self.adjust_shlvl_env(&mut prepared.env);
        }

        let argument = prepared.argv.last().cloned().unwrap_or_default();

        if prepared.argv.first().map(String::as_str) == Some("exec") {
            if !prepared.actions.is_empty()
                && let Err(message) = apply_actions(&prepared.actions)
            {
                eprintln!("sharky: {message}");
                return (Flow::Status(1), Some(argument));
            }
            self.ensure_child_env(&mut prepared);
            return (
                self.exec_builtin(&prepared.argv[1..], &prepared.env),
                Some(argument),
            );
        }

        if prepared.argv.is_empty() {
            if prepared.actions.is_empty() {
                return (Flow::Status(0), Some(argument));
            }
        } else if self.funcs.contains_key(&prepared.argv[0]) || is_builtin_name(&prepared.argv[0]) {
            if prepared.argv[0] == "command" {
                self.ensure_child_env(&mut prepared);
            }
            let argv = &prepared.argv;
            let flow = self.with_actions(&prepared.actions, |shell| {
                shell.run_in_shell(argv, &prepared.declarations, &prepared.env)
            });
            return (flow, Some(argument));
        }

        self.ensure_child_env(&mut prepared);
        if self.autocd_enabled()
            && prepared.argv.len() == 1
            && simple.assignments.is_empty()
            && prepared.actions.is_empty()
        {
            let candidate = &prepared.argv[0];
            let target = if Path::new(candidate).is_absolute() {
                PathBuf::from(candidate)
            } else {
                self.cwd.join(candidate)
            };
            if (candidate.contains('/')
                || candidate == "."
                || candidate == ".."
                || exec::resolve(candidate, self).is_none())
                && target.is_dir()
            {
                let status = self.cd(&["--".to_string(), candidate.to_string()]);
                return (Flow::Status(status), Some(argument));
            }
        }
        let command = prepared.argv.join(" ");
        let flow = match self.spawn(
            &prepared.argv,
            &prepared.actions,
            &prepared.env,
            SpawnChild::default(),
        ) {
            Ok(pid) => Flow::Status(self.wait_foreground(&[pid], pid, command)),
            Err(message) => {
                eprintln!("sharky: {message}");
                Flow::Status(1)
            }
        };
        (flow, Some(argument))
    }

    fn remember_underscore(&mut self, value: String) {
        if !self.is_readonly("_")
            && !self.assoc.contains_key("_")
            && !self.arrays.contains_key("_")
            && let Some((_, slot)) = self.env.iter_mut().rev().find(|(key, _)| key == "_")
        {
            *slot = value;
            self.exported.remove("_");
            return;
        }
        self.set_var("_", value);
        // Bash's `_` update clears the export attribute, and `export _` never
        // makes it exported; every child still gets it as the resolved path.
        self.exported.remove("_");
    }

    fn adjust_shlvl_env(&self, env: &mut Vec<(String, String)>) {
        let level = (self.shlvl_level - 1).max(0).to_string();
        set_env_entry(env, "SHLVL", level);
    }

    // An assignment-only command keeps its assignments in the current shell.
    // Its redirections apply to that command alone, but they are performed here
    // so a failed one reports its status without dropping the assignments,
    // which bash still applies. Assignment words expand before the redirections
    // are applied, so an expansion failure creates and assigns nothing.
    fn run_assignments(&mut self, simple: &Simple) -> Flow {
        let flow = self.run_assignments_inner(simple);
        self.remember_underscore(String::new());
        flow
    }

    fn run_assignments_inner(&mut self, simple: &Simple) -> Flow {
        if simple.redirects.is_empty() {
            self.substitution_status = None;
            let mut assigned = true;
            for assignment in &simple.assignments {
                assigned &= self.assign(assignment);
            }
            let failure = self.take_expansion_failure();
            let substitution = self.substitution_status.take().filter(|code| *code != 0);
            if let Some(flow) = failure {
                return flow;
            }
            if !assigned {
                return self.readonly_failure();
            }
            return match substitution {
                Some(code) => Flow::Status(code),
                None => Flow::Status(0),
            };
        }

        let actions = match self.redirect_actions(&simple.redirects) {
            Ok(actions) => actions,
            Err(message) => {
                if let Some(flow) = self.take_expansion_failure() {
                    return flow;
                }
                if !message.is_empty() {
                    eprintln!("sharky: {message}");
                }
                self.substitution_status = None;
                let mut assigned = true;
                for assignment in &simple.assignments {
                    assigned &= self.assign(assignment);
                }
                if let Some(flow) = self.take_expansion_failure() {
                    return flow;
                }
                if !assigned {
                    return self.readonly_failure();
                }
                return Flow::Status(1);
            }
        };

        let saved = match save_fds(&actions) {
            Ok(saved) => saved,
            Err(message) => {
                eprintln!("sharky: {message}");
                return Flow::Status(1);
            }
        };

        self.substitution_status = None;
        let mut assigned = true;
        for assignment in &simple.assignments {
            assigned &= self.assign(assignment);
        }
        let failure = self.take_expansion_failure();
        let substitution = self.substitution_status.take().filter(|code| *code != 0);
        let applied = apply_actions(&actions);
        restore_fds(saved);

        if let Some(flow) = failure {
            return flow;
        }
        if !assigned {
            return self.readonly_failure();
        }
        if let Err(message) = applied {
            eprintln!("sharky: {message}");
            return Flow::Status(1);
        }
        match substitution {
            Some(code) => Flow::Status(code),
            None => Flow::Status(0),
        }
    }

    // Runs a command that must change the shell itself. The redirections apply
    // to this command only, so the descriptors are saved, applied, and put back
    // even when the command or a redirection fails.
    fn with_actions<F>(&mut self, actions: &[Action], run: F) -> Flow
    where
        F: FnOnce(&mut Self) -> Flow,
    {
        if actions.is_empty() {
            return run(self);
        }
        let saved = match save_fds(actions) {
            Ok(saved) => saved,
            Err(message) => {
                eprintln!("sharky: {message}");
                return Flow::Status(1);
            }
        };

        if let Err(message) = apply_actions(actions) {
            restore_fds(saved);
            eprintln!("sharky: {message}");
            return Flow::Status(1);
        }

        let flow = run(self);
        restore_fds(saved);
        flow
    }

    fn sync_call_stack_arrays(&mut self) {
        if self.bash_source.is_empty() {
            self.arrays.remove("BASH_SOURCE");
        } else {
            self.arrays.insert(
                "BASH_SOURCE".to_string(),
                self.bash_source.iter().cloned().map(Some).collect(),
            );
        }
        if self.funcname.is_empty() {
            self.arrays.remove("FUNCNAME");
        } else {
            self.arrays.insert(
                "FUNCNAME".to_string(),
                self.funcname.iter().cloned().map(Some).collect(),
            );
        }
        if self.bash_lineno.is_empty() {
            self.arrays.remove("BASH_LINENO");
        } else {
            self.arrays.insert(
                "BASH_LINENO".to_string(),
                self.bash_lineno
                    .iter()
                    .map(|line| Some(line.to_string()))
                    .collect(),
            );
        }
    }

    fn invoke_function(&mut self, name: &str, body: &List, args: &[String]) -> Flow {
        let saved = std::mem::replace(&mut self.positional, args.to_vec());
        self.local_scopes.push(Vec::new());
        let pushed_main = if !self.bash_source.is_empty() && self.funcname.is_empty() {
            self.funcname.push("main".to_string());
            true
        } else {
            false
        };
        self.funcname.insert(0, name.to_string());
        let source_file = self
            .func_sources
            .get(name)
            .cloned()
            .or_else(|| self.bash_source.first().cloned())
            .unwrap_or_else(|| self.script.clone().unwrap_or_else(|| "sharky".to_string()));
        self.bash_source.insert(0, source_file);
        self.bash_lineno.insert(0, self.current_line.max(1));
        self.sync_call_stack_arrays();

        let saved_line = self.current_line;
        let flow = self.run_list(body);
        if matches!(flow, Flow::Status(_) | Flow::Return(_)) {
            let ret_status = match &flow {
                Flow::Return(code) => *code,
                Flow::Status(code) => *code,
                _ => self.last_status,
            };
            self.maybe_fire_return_trap(ret_status);
        }

        self.current_line = saved_line;
        if !self.bash_lineno.is_empty() {
            self.bash_lineno.remove(0);
        }
        if !self.bash_source.is_empty() {
            self.bash_source.remove(0);
        }
        if !self.funcname.is_empty() {
            self.funcname.remove(0);
        }
        if pushed_main && self.funcname.last().is_some_and(|n| n == "main") {
            self.funcname.pop();
        }
        self.sync_call_stack_arrays();
        self.restore_locals();
        self.positional = saved;
        match flow {
            Flow::Return(code) => Flow::Status(code),
            other => other,
        }
    }

    fn run_in_shell(
        &mut self,
        argv: &[String],
        declarations: &[Assignment],
        env: &[(String, String)],
    ) -> Flow {
        if let Some(body) = self.funcs.get(&argv[0]).cloned() {
            return self.invoke_function(&argv[0], &body, &argv[1..]);
        }

        match self.builtin(argv, declarations, env) {
            Some(flow) => flow,
            None => {
                eprintln!("sharky: command not found: {}", argv[0]);
                if self.interactive
                    && !self.session_mode
                    && let Some(suggestion) = self.suggest_similar_command(&argv[0])
                {
                    eprintln!("sharky: did you mean '{suggestion}'?");
                }
                Flow::Status(127)
            }
        }
    }

    // Runs in a forked child: never forks again for a simple command, so pipe
    // write ends close when the child exits. `merge_stderr` implements the
    // implicit `2>&1` of a `|&` link. Bash appends that dup after all of the
    // stage's own redirections, so a redirected compound stage receives the
    // flag through `run_command` rather than having stderr moved here.
    fn run_command_child(&mut self, command: &Command, merge_stderr: bool) -> i32 {
        match command {
            Command::Simple(simple) => self.run_simple_child(simple, merge_stderr),
            _ => flow_status(self.run_command_with_merge(command, merge_stderr)),
        }
    }

    // Applies the implicit `2>&1` of a `|&` stage after the command's own
    // redirections. `run_redirected` is the one place that knows the full
    // redirection sequence, so that is where the dup is appended.
    fn run_command_with_merge(&mut self, command: &Command, merge_stderr: bool) -> Flow {
        match command {
            Command::Redirected {
                command,
                redirects,
            } => self.run_redirected(command, redirects, merge_stderr),
            other => {
                // With no explicit redirection list the implicit dup is the
                // first thing applied to the stage, before its commands run.
                if merge_stderr
                    && move_fd(1, 2).is_err()
                {
                    return Flow::Status(1);
                }
                self.run_command(other)
            }
        }
    }

    fn run_simple_child(&mut self, simple: &Simple, merge_stderr: bool) -> i32 {
        let substitutions = self.substitution_fds.len();
        let status = self.run_simple_child_inner(simple, merge_stderr);
        self.close_substitutions_from(substitutions);
        status
    }

    fn run_simple_child_inner(&mut self, simple: &Simple, merge_stderr: bool) -> i32 {
        if simple.line > 0 && !self.trap_running {
            self.current_line = simple.line;
        }
        if simple.words.is_empty() && simple.redirects.is_empty() {
            self.substitution_status = None;
            let mut assigned = true;
            for assignment in &simple.assignments {
                assigned &= self.assign(assignment);
            }
            if self.nounset_failed {
                return 1;
            }
            if !assigned {
                // A forked child or subshell exits; the interactive flag is
                // meaningless here because no reader loop owns this process.
                return 1;
            }
            return self.substitution_status.take().unwrap_or(0);
        }

        let mut prepared = match self.prepare(simple) {
            Ok(prepared) => prepared,
            Err(message) => {
                if !message.is_empty() {
                    eprintln!("sharky: {message}");
                }
                return 1;
            }
        };

        if self.nounset_failed {
            return 1;
        }

        if let Err(message) = apply_actions(&prepared.actions) {
            eprintln!("sharky: {message}");
            return 1;
        }

        // `|&` appends `2>&1` after the command's own redirections, matching
        // bash's left-to-right processing: an explicit `>&2` in the left stage
        // therefore still reaches the original stderr.
        if merge_stderr
            && move_fd(1, 2).is_err()
        {
            return 1;
        }

        if prepared.argv.is_empty() {
            return 0;
        }

        if prepared.argv[0] == "exec" {
            self.ensure_child_env(&mut prepared);
            return flow_status(self.exec_builtin(&prepared.argv[1..], &prepared.env));
        }

        if let Some(body) = self.funcs.get(&prepared.argv[0]).cloned() {
            return flow_status(self.invoke_function(&prepared.argv[0], &body, &prepared.argv[1..]));
        }

        if prepared.argv[0] == "command" {
            self.ensure_child_env(&mut prepared);
        }
        if let Some(flow) = self.builtin(&prepared.argv, &prepared.declarations, &prepared.env) {
            return flow_status(flow);
        }

        self.ensure_child_env(&mut prepared);
        match exec::resolve(&prepared.argv[0], self) {
            Some(path) => exec::exec_replace(&path, &prepared.argv, &prepared.env),
            None => {
                eprintln!("sharky: command not found: {}", prepared.argv[0]);
                if self.interactive
                    && !self.session_mode
                    && let Some(suggestion) = self.suggest_similar_command(&prepared.argv[0])
                {
                    eprintln!("sharky: did you mean '{suggestion}'?");
                }
                127
            }
        }
    }

    fn exec_builtin(&mut self, args: &[String], env: &[(String, String)]) -> Flow {
        let mut index = 0;
        let mut name = None;

        if args.first().map(String::as_str) == Some("-a") {
            match args.get(1) {
                Some(value) => {
                    name = Some(value.clone());
                    index = 2;
                }
                None => {
                    eprintln!("sharky: exec: -a: option requires an argument");
                    return Flow::Status(1);
                }
            }
        }

        let Some(program) = args.get(index) else {
            return Flow::Status(0);
        };

        // A session cannot let `exec` replace the interpreter: the protocol
        // would die mid-response. The redirect-only form (`exec 3>f`,
        // `exec >f`) never gets here — it has no program and was applied
        // around this builtin — so it keeps working in process.
        if self.session_mode {
            let _ = crate::term::write_stderr(
                "sharky: exec: process replacement is not available in agent session mode\n",
            );
            return Flow::Status(126);
        }

        let rest = &args[index + 1..];

        let Some(path) = exec::resolve(program, self) else {
            eprintln!("sharky: exec: {program}: not found");
            return self.exec_failure(127);
        };
        if !path.exists() {
            eprintln!("sharky: exec: {program}: not found");
            return self.exec_failure(127);
        }

        let mut argv = Vec::with_capacity(rest.len() + 1);
        argv.push(name.unwrap_or_else(|| program.clone()));
        argv.extend_from_slice(rest);

        // Bash's explicit `exec` exports one shell level less, except inside a
        // subshell, where the normal export value is kept.
        let mut env = env.to_vec();
        if self.in_subshell {
            if self.exported.contains("SHLVL") {
                set_env_entry(&mut env, "SHLVL", self.shlvl_level.to_string());
            } else {
                env.retain(|(name, _)| name != "SHLVL");
            }
        } else {
            self.adjust_shlvl_env(&mut env);
        }

        self.publish_json_exec();
        match exec::exec_now_plain(&path, &argv, &env) {
            Ok(()) => Flow::Status(0),
            Err(err) => {
                eprintln!("sharky: exec: {program}: {err}");
                self.exec_failure(exec::exec_failure_status(err))
            }
        }
    }

    // Bash reports a null or unset parameter error with 127 for a -c command
    // string, but with 1 in a script, on standard input, in a subshell, in an
    // interactive shell or in a shell with errexit enabled. Arithmetic and
    // indirect errors always use 1.
    fn unset_parameter_status(&self) -> i32 {
        if self.command_string
            && !self.interactive
            && !self.errexit
            && !self.script.is_some()
            && !self.in_subshell
        {
            127
        } else {
            1
        }
    }

    // A failing expansion in an interactive shell discards the rest of the
    // input unit and reports its status; bash throws back to the reader loop
    // rather than continuing with the commands that follow on the same line.
    fn unset_parameter_failure(&mut self) -> Flow {
        let code = self.unset_parameter_status();
        self.nounset_failed = false;
        self.null_parameter_error = false;
        if self.interactive {
            Flow::Discard(code)
        } else {
            self.exit_requested = Some(code);
            Flow::Exit(code)
        }
    }

    fn expansion_failure(&mut self) -> Flow {
        let code = if self.null_parameter_error {
            self.unset_parameter_status()
        } else {
            1
        };
        self.null_parameter_error = false;
        self.expansion_error = false;
        if self.interactive {
            Flow::Discard(code)
        } else {
            Flow::Exit(code)
        }
    }

    // Consumes a pending expansion failure at the command that produced it.
    // Clearing the flags here keeps them from aborting a later command, which
    // is what bash reports: the failing command, not the one after it.
    fn take_expansion_failure(&mut self) -> Option<Flow> {
        if self.nounset_failed {
            return Some(self.unset_parameter_failure());
        }
        if self.expansion_error {
            return Some(self.expansion_failure());
        }
        None
    }

    fn exec_failure(&self, code: i32) -> Flow {
        if self.interactive {
            Flow::Status(code)
        } else {
            Flow::Exit(code)
        }
    }

    /// Waits for a subshell or redirected child. A job-control stop is only an
    /// event for the shell that owns the terminal and the job table; a nested
    /// shell keeps waiting for the process to finish, because the outer shell
    /// reports and resumes the whole group.
    fn wait_subshell(&mut self, child: Pid) -> Option<WaitStatus> {
        self.wait_foreground_member(child)
    }

    /// Waits for one member of the foreground job, ignoring stop statuses when
    /// this shell is itself part of a job. A nested shell must not consume the
    /// stop and turn it into an exit status while its parent waits for it.
    fn wait_foreground_member(&mut self, pid: Pid) -> Option<WaitStatus> {
        loop {
            match exec::wait_traced(pid) {
                Some(WaitStatus::Stopped(_, _)) if !self.foreground_control() => continue,
                other => return other,
            }
        }
    }

    fn run_subshell(&mut self, list: &List) -> Flow {
        let mask = crate::signal::block_all();
        // SAFETY: the child runs shell code and exits without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                if (self.interactive || self.isolate_groups) && self.job_pgid.is_none() {
                    // Both sides set the group: the child's own call and this
                    // one race, and closing it here means the terminal never
                    // hands foreground status to a group that does not exist
                    // yet. Inside a background job the child joins the job's
                    // existing group instead.
                    let _ = setpgid(child, child);
                    crate::agent::note_foreground(child.as_raw());
                    self.set_foreground(child);
                }
                // A stopped subshell must come back to the prompt, not block
                // this wait forever, and must stay a job that `jobs`, `fg`,
                // `bg` and `kill %n` can still reach.
                let status = self.wait_subshell(child);
                crate::agent::clear_foreground();
                self.reclaim_terminal();
                match status {
                    Some(WaitStatus::Stopped(_, signal)) if self.foreground_control() => {
                        let command = format!("( {} )", describe_list(list));
                        self.record_stopped_job(child, command, signal as i32);
                        Flow::Status(128 + signal as i32)
                    }
                    Some(WaitStatus::Exited(_, code)) => Flow::Status(code),
                    Some(WaitStatus::Signaled(_, signal, _)) => Flow::Status(128 + signal as i32),
                    _ => Flow::Status(1),
                }
            }
            Ok(ForkResult::Child) => {
                if self.interactive || self.isolate_groups {
                    // A subshell leads its own job when it is the foreground
                    // command; inside a background job it stays in the group
                    // that job already has, so one signal reaches all of it.
                    let group = self.job_pgid.unwrap_or_else(getpid);
                    let _ = setpgid(Pid::from_raw(0), group);
                    self.job_pgid = Some(group);
                }
                crate::agent::child_guard();
                self.reset_child_traps();
                crate::signal::restore_mask(mask);
                self.in_subshell = true;
                // A subshell's last command is exec'd without a fork too, so
                // it exports one shell level less.
                self.shlvl_tail = final_simple_in_list(list);
                let code = flow_status(self.run_list(list));
                unsafe { _exit(code) }
            }
            Err(err) => {
                crate::signal::restore_mask(mask);
                eprintln!("sharky: fork: {err}");
                Flow::Status(1)
            }
        }
    }

    fn run_redirected(
        &mut self,
        command: &Command,
        redirects: &[Redirect],
        merge_stderr: bool,
    ) -> Flow {
        let mut actions = match self.redirect_actions(redirects) {
            Ok(actions) => actions,
            Err(message) => {
                eprintln!("sharky: {message}");
                return Flow::Status(1);
            }
        };
        // Bash's `|&` is `2>&1 |`: the implicit dup is appended to the stage's
        // own redirections, so it sees the stage's final stdout destination.
        if merge_stderr {
            actions.push(Action::Dup { from: 1, to: 2 });
        }

        if let Command::Simple(simple) = command
            && simple.words.first().and_then(literal).as_deref() == Some("exec")
        {
            if let Err(message) = apply_actions(&actions) {
                eprintln!("sharky: {message}");
                return Flow::Status(1);
            }
            let mut prepared = match self.prepare(simple) {
                Ok(prepared) => prepared,
                Err(message) => {
                    eprintln!("sharky: {message}");
                    return Flow::Status(1);
                }
            };
            self.ensure_child_env(&mut prepared);
            return self.exec_builtin(&prepared.argv[1..], &prepared.env);
        }

        // A group shares the shell's state, so its redirections are temporary
        // rather than a fork boundary.
        if matches!(command, Command::Group(_)) {
            return self.with_actions(&actions, |shell| shell.run_command(command));
        }

        let mask = crate::signal::block_all();
        // SAFETY: the child runs shell code and exits without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                if (self.interactive || self.isolate_groups) && self.job_pgid.is_none() {
                    let _ = setpgid(child, child);
                    crate::agent::note_foreground(child.as_raw());
                    self.set_foreground(child);
                }
                let status = self.wait_subshell(child);
                crate::agent::clear_foreground();
                self.reclaim_terminal();
                match status {
                    Some(WaitStatus::Stopped(_, signal)) if self.foreground_control() => {
                        self.record_stopped_job(child, describe_command(command), signal as i32);
                        Flow::Status(128 + signal as i32)
                    }
                    Some(WaitStatus::Exited(_, code)) => Flow::Status(code),
                    Some(WaitStatus::Signaled(_, signal, _)) => Flow::Status(128 + signal as i32),
                    _ => Flow::Status(1),
                }
            }
            Ok(ForkResult::Child) => {
                if self.interactive || self.isolate_groups {
                    let group = self.job_pgid.unwrap_or_else(getpid);
                    let _ = setpgid(Pid::from_raw(0), group);
                    self.job_pgid = Some(group);
                }
                crate::agent::child_guard();
                self.reset_child_traps();
                crate::signal::restore_mask(mask);
                if let Err(message) = apply_actions(&actions) {
                    eprintln!("sharky: {message}");
                    unsafe { _exit(1) }
                }
                let code = self.run_command_child(command, false);
                unsafe { _exit(code) }
            }
            Err(err) => {
                crate::signal::restore_mask(mask);
                eprintln!("sharky: fork: {err}");
                Flow::Status(1)
            }
        }
    }

    fn run_if(&mut self, branches: &[(List, List)], else_body: Option<&List>) -> Flow {
        for (condition, body) in branches {
            // A condition whose expansion fails ends the whole compound command:
            // no branch runs, not even else, and the discard flow carries the
            // error status out of the if.
            match self.run_suppressed(condition) {
                Flow::Status(0) => return self.run_list(body),
                Flow::Status(_) => {}
                other => return other,
            }
        }

        match else_body {
            Some(body) => self.run_list(body),
            None => Flow::Status(0),
        }
    }

    fn run_for(&mut self, name: &str, words: &[Word], body: &List) -> Flow {
        let values: Vec<String> = if words.is_empty() {
            self.positional.clone()
        } else {
            let mut values = Vec::new();
            for word in words {
                values.extend(self.expand_word(word));
            }
            values
        };

        // A word list that failed to expand aborts the whole loop: bash runs no
        // iteration, not even for the words expanded before the failure.
        if let Some(flow) = self.take_expansion_failure() {
            return flow;
        }

        // A readonly loop variable reports and stops the loop before any
        // iteration, with status 1; the script goes on.
        if self.is_readonly(name) {
            self.readonly_write_error(name);
            return Flow::Status(1);
        }

        let mut status = 0;
        for value in values {
            self.set_var(name, value);
            match self.run_list(body) {
                Flow::Status(code) => status = code,
                Flow::Break => break,
                Flow::Continue => continue,
                other => return other,
            }
        }

        Flow::Status(status)
    }

    fn run_for_arith(&mut self, init: &str, condition: &str, step: &str, body: &List) -> Flow {
        if !init.is_empty()
            && let Err(error) = arith::evaluate(init, self)
        {
            return Flow::Status(arithmetic_failure(error));
        }

        let mut status = 0;
        loop {
            if !condition.is_empty() {
                match arith::evaluate(condition, self) {
                    Ok(0) => break,
                    Ok(_) => {}
                    // A refused readonly write reports and reads as false.
                    Err(arith::ArithError::Readonly) => break,
                    Err(error) => return Flow::Status(arithmetic_failure(error)),
                }
            }

            match self.run_list(body) {
                Flow::Status(code) => status = code,
                Flow::Break => break,
                Flow::Continue => {}
                other => return other,
            }

            if !step.is_empty()
                && let Err(error) = arith::evaluate(step, self)
            {
                return Flow::Status(arithmetic_failure(error));
            }
        }

        Flow::Status(status)
    }

    fn run_while(&mut self, condition: &List, body: &List, until: bool) -> Flow {
        let mut status = 0;

        loop {
            let condition_status = match self.run_suppressed(condition) {
                Flow::Status(code) => code,
                other => return other,
            };

            let keep_going = if until {
                condition_status != 0
            } else {
                condition_status == 0
            };
            if !keep_going {
                break;
            }

            match self.run_list(body) {
                Flow::Status(code) => status = code,
                Flow::Break => break,
                Flow::Continue => continue,
                other => return other,
            }
        }

        Flow::Status(status)
    }

    fn run_case(&mut self, word: &Word, arms: &[CaseArm]) -> Flow {
        let value = self.expand_text_word(word);
        if let Some(flow) = self.take_expansion_failure() {
            return flow;
        }
        let mut status = 0;
        let mut index = 0;
        let mut matched = false;
        let mut run_next = false;

        while index < arms.len() {
            let (patterns, body, terminator) = &arms[index];
            let matches = if run_next {
                true
            } else if matched {
                false
            } else {
                let mut arm_matches = false;
                for pattern in patterns {
                    let pattern = self.expand_pattern_word(pattern);
                    if let Some(flow) = self.take_expansion_failure() {
                        return flow;
                    }
                    if glob_match_opts(&pattern, &value, self.extglob, self.nocasematch) {
                        arm_matches = true;
                        break;
                    }
                }
                arm_matches
            };

            if !matches {
                index += 1;
                continue;
            }

            match self.run_list(body) {
                Flow::Status(code) => status = code,
                Flow::Break => return Flow::Break,
                Flow::Continue => return Flow::Continue,
                other => return other,
            }

            match terminator {
                CaseTerm::Break => break,
                CaseTerm::Fallthrough => {
                    matched = false;
                    run_next = true;
                    index += 1;
                }
                CaseTerm::Continue => {
                    matched = false;
                    run_next = false;
                    index += 1;
                }
            }
        }

        Flow::Status(status)
    }

    fn prepare(&mut self, simple: &Simple) -> Result<Prepared, String> {
        let mut argv = Vec::new();
        for word in &simple.words {
            argv.extend(self.expand_word(word));
        }

        if self.expansion_error {
            self.expansion_error = false;
            return Err(String::new());
        }

        if self.xtrace && !argv.is_empty() {
            eprintln!("+ {}", argv.join(" "));
        }

        let actions = self.redirect_actions(&simple.redirects)?;

        let has_env = !simple.assignments.is_empty();
        let mut env = if has_env {
            self.child_env()
        } else {
            Vec::new()
        };
        let mut declarations = Vec::new();
        let declaring = argv.first().is_some_and(|name| is_declaration_word(name));
        for assignment in &simple.assignments {
            // A refused prefix assignment reports, drops the value and still
            // runs the command, which owns the status.
            if self.is_readonly(&assignment.name) {
                self.readonly_write_error(&assignment.name);
                continue;
            }
            match &assignment.value {
                AssignmentValue::Array(_) if declaring => declarations.push(assignment.clone()),
                AssignmentValue::Array(_) => {
                    self.assign(assignment);
                }
                AssignmentValue::Word(_) if assignment.index.is_some() => {
                    self.assign(assignment);
                }
                AssignmentValue::Word(word) => {
                    let value = self.expand_assignment(word);
                    match env.iter_mut().find(|(key, _)| key == &assignment.name) {
                        Some(entry) => entry.1 = value,
                        None => env.push((assignment.name.clone(), value)),
                    }
                }
            }
        }

        if self.expansion_error {
            self.expansion_error = false;
            return Err(String::new());
        }

        Ok(Prepared {
            argv,
            actions,
            env,
            has_env,
            declarations,
        })
    }

    fn ensure_child_env(&self, prepared: &mut Prepared) {
        if !prepared.has_env {
            prepared.env = self.child_env();
            prepared.has_env = true;
        }
    }

    // shortcut: aliases expand to plain words; pipes and redirections in an
    // alias value are not supported yet.
    fn expand_alias(&mut self, expansion: &str, extra: &[String]) -> Option<Vec<String>> {
        let tokens = lexer::tokenize(expansion).ok()?;
        let mut argv = Vec::new();

        for token in tokens {
            match token.kind {
                TokenKind::Word(word) => {
                    argv.extend(self.expand_word(&word));
                }
                TokenKind::Eof => break,
                _ => return None,
            }
        }

        argv.extend_from_slice(extra);
        Some(argv)
    }

    fn redirect_actions(&mut self, redirects: &[Redirect]) -> Result<Vec<Action>, String> {
        let mut actions = Vec::new();

        for redirect in redirects {
            match &redirect.op {
                RedirectOp::Dup(from) => actions.push(Action::Dup {
                    from: *from,
                    to: redirect.fd,
                }),
                RedirectOp::DupIn(from) => actions.push(Action::Dup {
                    from: *from,
                    to: redirect.fd,
                }),
                RedirectOp::DupWord | RedirectOp::DupInWord => {
                    let Some(word) = &redirect.target else {
                        return Err("redirect without a target".to_string());
                    };
                    let fields = self.expand_word(word);
                    if fields.len() != 1 {
                        return Err("ambiguous redirect".to_string());
                    }
                    let spec = fields[0].as_str();
                    if spec == "-" {
                        actions.push(Action::Close(redirect.fd));
                    } else if let Some(num) = spec.strip_suffix('-')
                        && let Ok(from) = num.parse::<i32>()
                    {
                        actions.push(Action::Dup {
                            from,
                            to: redirect.fd,
                        });
                        actions.push(Action::Close(from));
                    } else if let Ok(from) = spec.parse::<i32>() {
                        actions.push(Action::Dup {
                            from,
                            to: redirect.fd,
                        });
                    } else {
                        return Err(format!("{spec}: ambiguous redirect"));
                    }
                }
                RedirectOp::Close => actions.push(Action::Close(redirect.fd)),
                RedirectOp::Here { body, expand } => {
                    let text = if *expand {
                        let word =
                            lexer::heredoc_word(body).map_err(|err| err.message)?;
                        self.expand_text_word(&word)
                    } else {
                        body.clone()
                    };
                    actions.push(Action::Here {
                        fd: redirect.fd,
                        file: temp_file(&text)?,
                    });
                }
                RedirectOp::HereString => {
                    let Some(word) = &redirect.target else {
                        return Err("redirect without a target".to_string());
                    };
                    let text = format!("{}\n", self.expand_text_word(word));
                    actions.push(Action::Here {
                        fd: redirect.fd,
                        file: temp_file(&text)?,
                    });
                }
                RedirectOp::Both { append } => {
                    let Some(word) = &redirect.target else {
                        return Err("redirect without a target".to_string());
                    };
                    let fields = self.expand_word(word);
                    if fields.len() != 1 {
                        return Err("ambiguous redirect".to_string());
                    }
                    let path = PathBuf::from(&fields[0]);
                    actions.push(if *append {
                        Action::Append(1, path)
                    } else {
                        Action::Write(1, path, self.noclobber)
                    });
                    actions.push(Action::Dup { from: 1, to: 2 });
                }
                op => {
                    let Some(word) = &redirect.target else {
                        return Err("redirect without a target".to_string());
                    };
                    let fields = self.expand_word(word);
                    if fields.len() != 1 {
                        return Err("ambiguous redirect".to_string());
                    }
                    let path = PathBuf::from(&fields[0]);
                    actions.push(match op {
                        RedirectOp::Out => Action::Write(redirect.fd, path, self.noclobber),
                        RedirectOp::OutForce => Action::Write(redirect.fd, path, false),
                        RedirectOp::Append => Action::Append(redirect.fd, path),
                        RedirectOp::In => Action::Read(redirect.fd, path),
                        RedirectOp::Dup(_)
                        | RedirectOp::DupIn(_)
                        | RedirectOp::DupWord
                        | RedirectOp::DupInWord
                        | RedirectOp::Close
                        | RedirectOp::Here { .. }
                        | RedirectOp::HereString
                        | RedirectOp::Both { .. } => unreachable!(),
                    });
                }
            }
        }

        if self.expansion_error {
            return Err(String::new());
        }

        Ok(actions)
    }

    fn spawn(
        &mut self,
        argv: &[String],
        actions: &[Action],
        env: &[(String, String)],
        child: SpawnChild<'_>,
    ) -> Result<Pid, String> {
        let SpawnChild {
            stdin,
            stdout,
            leader,
            program,
        } = child;
        let resolved = match program {
            Some(path) => Some(path.to_path_buf()),
            None => argv.first().and_then(|name| exec::resolve(name, self)),
        };
        let hint = if resolved.is_none() && self.interactive && !self.session_mode {
            argv.first().and_then(|cmd| self.suggest_similar_command(cmd))
        } else {
            None
        };
        // SAFETY: the child only runs syscalls and allocation-safe code before exec.
        let mask = crate::signal::block_all();
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                if leader.is_none() {
                    // The child sets its own group too; doing it here as well
                    // closes the window where the hand-off fails with ESRCH
                    // because the child has not run yet. Inside a subshell the
                    // command joins the subshell's group instead.
                    if (self.interactive || self.isolate_groups) && self.job_pgid.is_none() {
                        let _ = setpgid(child, child);
                        crate::agent::note_foreground(child.as_raw());
                    }
                    self.set_foreground(child);
                }
                Ok(child)
            }
            Ok(ForkResult::Child) => {
                if self.interactive || self.isolate_groups {
                    let target = self.job_pgid.or(leader).unwrap_or(Pid::from_raw(0));
                    let _ = setpgid(Pid::from_raw(0), target);
                }
                crate::agent::child_guard();
                self.reset_child_traps();
                crate::signal::restore_mask(mask);
                if let Some(fd) = stdin
                    && move_fd(fd, 0).is_err()
                {
                    unsafe { _exit(126) }
                }
                if let Some(fd) = stdout
                    && move_fd(fd, 1).is_err()
                {
                    unsafe { _exit(126) }
                }
                if let Err(message) = apply_actions(actions) {
                    eprintln!("sharky: {message}");
                    unsafe { _exit(1) }
                }
                if argv.is_empty() {
                    unsafe { _exit(0) }
                }
                if let Some(flow) = self.builtin(argv, &[], env) {
                    unsafe { _exit(flow_status(flow)) }
                }
                match resolved {
                    Some(path) => exec::exec_replace(&path, argv, env),
                    None => {
                        eprintln!("sharky: command not found: {}", argv[0]);
                        if let Some(suggestion) = hint {
                            eprintln!("sharky: did you mean '{suggestion}'?");
                        }
                        unsafe { _exit(127) }
                    }
                }
            }
            Err(err) => {
                crate::signal::restore_mask(mask);
                Err(format!("fork: {err}"))
            }
        }
    }

    fn builtin(
        &mut self,
        words: &[String],
        declarations: &[Assignment],
        env: &[(String, String)],
    ) -> Option<Flow> {
        match words[0].as_str() {
            ":" | "true" => Some(Flow::Status(0)),
            "false" => Some(Flow::Status(1)),
            "exit" => {
                let code = words
                    .get(1)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(self.last_status);
                self.exit_requested = Some(code);
                Some(Flow::Exit(code))
            }
            "return" => {
                // Bash accepts `return` only in a function or a sourced file.
                // Elsewhere it reports an error, the command's status becomes
                // 2, and the rest of the input unit keeps running.
                if self.local_scopes.is_empty() && self.source_depth == 0 {
                    eprintln!(
                        "sharky: return: can only `return' from a function or sourced script"
                    );
                    return Some(Flow::Status(2));
                }
                Some(Flow::Return(
                    words
                        .get(1)
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(self.last_status),
                ))
            }
            "break" => Some(Flow::Break),
            "continue" => Some(Flow::Continue),
            "cd" => Some(Flow::Status(self.cd(&words[1..]))),
            "pushd" => Some(Flow::Status(self.pushd(&words[1..]))),
            "popd" => Some(Flow::Status(self.popd(&words[1..]))),
            "dirs" => Some(Flow::Status(self.dirs(&words[1..]))),
            "ulimit" => Some(Flow::Status(self.ulimit(&words[1..]))),
            "hash" => Some(Flow::Status(hash_builtin(self, &words[1..]))),
            "let" => Some(Flow::Status(self.let_builtin(&words[1..]))),
            "times" => Some(Flow::Status(times_builtin())),
            "export" => Some(Flow::Status(self.export(&words[1..]))),
            "unset" => Some(Flow::Status(self.unset(&words[1..]))),
            "set" => Some(Flow::Status(self.set(&words[1..]))),
            "shopt" => Some(Flow::Status(self.shopt(&words[1..]))),
            "jobs" => Some(Flow::Status(self.list_jobs(&words[1..]))),
            "pwd" => Some(Flow::Status(self.pwd(&words[1..]))),
            "echo" => Some(Flow::Status(echo_builtin(&words[1..]))),
            "printf" => Some(Flow::Status(self.printf_builtin(&words[1..]))),
            "type" => Some(Flow::Status(self.type_builtin(&words[1..]))),
            "command" => Some(self.command_builtin(&words[1..], env)),
            "builtin" => Some(self.builtin_cmd(&words[1..], declarations, env)),
            "caller" => Some(Flow::Status(self.caller(&words[1..]))),
            "complete" => Some(Flow::Status(self.complete_builtin(&words[1..]))),
            "compgen" => Some(Flow::Status(self.compgen_builtin(&words[1..]))),
            "prevd" => Some(Flow::Status(self.prevd_builtin(&words[1..]))),
            "nextd" => Some(Flow::Status(self.nextd_builtin(&words[1..]))),
            "from-json" => Some(Flow::Status(from_json_builtin(&words[1..]))),
            "to-json" => Some(Flow::Status(to_json_builtin(&words[1..]))),
            "where" => Some(Flow::Status(where_builtin(&words[1..]))),
            "select" => Some(Flow::Status(select_builtin(&words[1..]))),
            "table" => Some(Flow::Status(table_builtin(&words[1..]))),
            "help" => Some(Flow::Status(crate::help::builtin(&words[1..]))),
            "alias" => Some(Flow::Status(self.alias(&words[1..]))),
            "abbr" => Some(Flow::Status(self.abbr(&words[1..]))),
            "unabbr" => Some(Flow::Status(self.unabbr(&words[1..]))),
            "unalias" => Some(Flow::Status(self.unalias(&words[1..]))),
            "history" => Some(Flow::Status(self.history_builtin(&words[1..]))),
            "wake" => {
                if let Some(command) = words.get(1) {
                    self.wake.push(command.clone());
                }
                Some(Flow::Status(0))
            }
            "wait" => Some(Flow::Status(self.wait_builtin(&words[1..]))),
            "kill" => Some(Flow::Status(self.kill_builtin(&words[1..]))),
            "declare" => Some(Flow::Status(self.declaration(
                "declare",
                &words[1..],
                declarations,
            ))),
            "local" => Some(Flow::Status(self.declaration("local", &words[1..], declarations))),
            "readonly" => Some(Flow::Status(self.declaration(
                "readonly",
                &words[1..],
                declarations,
            ))),
            "typeset" => Some(Flow::Status(self.declaration(
                "typeset",
                &words[1..],
                declarations,
            ))),
            "z" => Some(Flow::Status(self.z_builtin(&words[1..], false))),
            "zi" => Some(Flow::Status(self.z_builtin(&words[1..], true))),
            "reload" => Some(Flow::Status(self.reload())),
            "trap" => Some(Flow::Status(self.trap(&words[1..]))),
            "read" => Some(Flow::Status(self.read(&words[1..], env))),
            "mapfile" | "readarray" => Some(Flow::Status(self.mapfile(&words[1..]))),
            "shift" => Some(Flow::Status(self.shift(&words[1..]))),
            "fg" => Some(Flow::Status(self.fg(&words[1..]))),
            "bg" => Some(Flow::Status(self.bg(&words[1..]))),
            "disown" => Some(Flow::Status(self.disown(&words[1..]))),
            "source" | "." => Some(self.source(&words[1..])),
            "test" => Some(Flow::Status(self.test_builtin(&words[1..], false))),
            "[" => Some(Flow::Status(self.test_builtin(&words[1..], true))),
            "eval" => Some(self.eval_builtin(&words[1..])),
            "umask" => Some(Flow::Status(self.umask_builtin(&words[1..]))),
            "getopts" => Some(Flow::Status(self.getopts_builtin(&words[1..]))),
            _ => None,
        }
    }

    fn resolve_nameref_ref<'a>(
        &'a self,
        name: &'a str,
    ) -> Result<(Cow<'a, str>, Option<String>), String> {
        if !self.namerefs.contains_key(name) {
            return Ok((Cow::Borrowed(name), None));
        }
        let mut current = Cow::Borrowed(name);
        let mut sub: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..64 {
            let Some(target_opt) = self.namerefs.get(current.as_ref()) else {
                return Ok((current, sub));
            };
            let Some(target) = target_opt else {
                return Ok((current, sub));
            };
            if target.is_empty() {
                return Ok((current, sub));
            }
            if !seen.insert(current.to_string()) {
                return Err(format!("sharky: {name}: circular name reference"));
            }
            if let Some((base, index)) = split_array_argument(target) {
                if sub.is_some() {
                    return Ok((Cow::Owned(target.clone()), sub));
                }
                current = Cow::Owned(base);
                sub = Some(unquote_subscript(&index));
            } else {
                current = Cow::Owned(target.clone());
            }
        }
        Err(format!("sharky: {name}: circular name reference"))
    }

    fn resolve_nameref(&self, name: &str) -> Result<(String, Option<String>), String> {
        let (target, sub) = self.resolve_nameref_ref(name)?;
        Ok((target.into_owned(), sub))
    }

    fn expansion_var(&mut self, name: &str) -> String {
        let (target, sub) = match self.resolve_nameref_ref(name) {
            Ok((target, sub)) => (target.into_owned(), sub),
            Err(err) => {
                eprintln!("{err}");
                self.expansion_error = true;
                return String::new();
            }
        };
        let unbound_nameref = self
            .namerefs
            .get(&target)
            .is_some_and(Option::is_none);
        if self.nounset
            && (unbound_nameref
                || (!is_special_name(&target)
                    && !self.arrays.contains_key(&target)
                    && !self.assoc.contains_key(&target)
                    && !self.env.iter().any(|(key, _)| key == &target)))
        {
            eprintln!("sharky: {name}: unbound variable");
            self.nounset_failed = true;
            return String::new();
        }
        if let Some(sub) = sub {
            if let Some(map) = self.assoc.get(&target) {
                return map.get(&sub).cloned().unwrap_or_default();
            }
            let idx = self.resolve_array_index(&sub);
            if let Some(values) = self.arrays.get(&target) {
                return values.get(idx).cloned().flatten().unwrap_or_default();
            }
            return if idx == 0 { self.var(&target) } else { String::new() };
        }
        self.var(&target)
    }

    fn field_separators(&self) -> String {
        match self.env.iter().find(|(key, _)| key == "IFS") {
            Some((_, value)) => value.clone(),
            None => " \t\n".to_string(),
        }
    }

    fn ifs_first_char(&self) -> Option<char> {
        match self.env.iter().find(|(key, _)| key == "IFS") {
            Some((_, value)) => value.chars().next(),
            None => Some(' '),
        }
    }

    fn ifs_join_separator(&self) -> String {
        self.ifs_first_char().map(String::from).unwrap_or_default()
    }

    /// Separator for an unquoted joined expansion: the first character of IFS.
    /// A null IFS leaves the elements as separate words, except for the keys
    /// of `${!a[*]}`, which bash keeps apart with a space.
    fn unquoted_join(&self, parameter: &Parameter) -> Option<String> {
        if !joins_elements(parameter) {
            return None;
        }
        match self.ifs_first_char() {
            Some(separator) => Some(separator.to_string()),
            None if parameter.operator == Some(ParamOp::Keys) => Some(" ".to_string()),
            None => None,
        }
    }

    pub fn var(&self, name: &str) -> String {
        let (target, sub) = match self.resolve_nameref_ref(name) {
            Ok(resolved) => resolved,
            Err(_) => return String::new(),
        };
        let name = target.as_ref();
        if self.namerefs.get(name).is_some_and(Option::is_none) {
            return String::new();
        }
        if let Some(sub) = sub {
            if let Some(map) = self.assoc.get(name) {
                return map.get(&sub).cloned().unwrap_or_default();
            }
            if let Some(values) = self.arrays.get(name) {
                return sub
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|idx| values.get(idx))
                    .cloned()
                    .flatten()
                    .unwrap_or_default();
            }
            if sub.trim() != "0" {
                return String::new();
            }
        }
        if !name.is_empty() && name.chars().all(|char| char.is_ascii_digit()) {
            if name == "0" {
                return self
                    .script
                    .clone()
                    .unwrap_or_else(|| "sharky".to_string());
            }
            return name
                .parse::<usize>()
                .ok()
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| self.positional.get(index))
                .cloned()
                .unwrap_or_default();
        }

        match name {
            "$" => return self.pid.to_string(),
            "?" => return self.last_status.to_string(),
            "#" => return self.positional.len().to_string(),
            "!" => {
                return self
                    .last_bg_pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_default();
            }
            "@" => return self.positional.join(" "),
            "*" => return self.positional.join(&self.ifs_join_separator()),
            "BASHPID" => return std::process::id().to_string(),
            "UID" => return nix::unistd::getuid().as_raw().to_string(),
            "EUID" => return nix::unistd::geteuid().as_raw().to_string(),
            "RANDOM" => {
                let next = self
                    .random_state
                    .get()
                    .wrapping_mul(1_103_515_245)
                    .wrapping_add(12_345);
                self.random_state.set(next);
                return ((next >> 16) & 0x7fff).to_string();
            }
            "SECONDS" => {
                let elapsed = self.seconds_origin.elapsed().as_secs() as i64;
                return (elapsed.saturating_add(self.seconds_offset)).to_string();
            }
            "LINENO" => {
                let line = (self.current_line as i64)
                    .saturating_add(self.lineno_offset.unwrap_or(0))
                    .max(0);
                return line.to_string();
            }
            _ => {}
        }

        if let Some(map) = self.assoc.get(name) {
            // A bare name of an associative array is its element under key 0.
            return map.get("0").cloned().unwrap_or_default();
        }

        if let Some(values) = self.arrays.get(name) {
            return values.first().cloned().flatten().unwrap_or_default();
        }

        self.env
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    fn scalar_is_set(&self, name: &str) -> bool {
        let (target, sub) = match self.resolve_nameref_ref(name) {
            Ok(resolved) => resolved,
            Err(_) => return false,
        };
        let name = target.as_ref();
        if self.namerefs.get(name).is_some_and(Option::is_none) {
            return false;
        }
        if let Some(sub) = sub {
            if let Some(map) = self.assoc.get(name) {
                return map.contains_key(&sub);
            }
            if let Some(values) = self.arrays.get(name) {
                return sub
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|idx| values.get(idx))
                    .is_some_and(Option::is_some);
            }
            if sub.trim() != "0" {
                return false;
            }
        }
        if name == "0" {
            return true;
        }
        if !name.is_empty() && name.chars().all(|char| char.is_ascii_digit()) {
            return name
                .parse::<usize>()
                .ok()
                .and_then(|index| index.checked_sub(1))
                .is_some_and(|index| index < self.positional.len());
        }
        match name {
            "@" | "*" => !self.positional.is_empty(),
            "!" => self.last_bg_pid.is_some(),
            "?" | "#" | "$" | "-" | "BASHPID" | "UID" | "EUID" | "RANDOM" | "SECONDS"
            | "LINENO" => true,
            _ => self.env.iter().any(|(key, _)| key == name),
        }
    }

    fn assign(&mut self, assignment: &Assignment) -> bool {
        if matches!(self.namerefs.get(&assignment.name), Some(None)) {
            if self.is_readonly(&assignment.name) {
                self.readonly_write_error(&assignment.name);
                return false;
            }
            if let AssignmentValue::Word(word) = &assignment.value
                && assignment.index.is_none()
                && !assignment.append
            {
                let value = self.expand_assignment(word);
                self.namerefs.insert(assignment.name.clone(), Some(value));
                return true;
            }
        }
        let (target_name, target_sub) = match self.resolve_nameref(&assignment.name) {
            Ok(resolved) => resolved,
            Err(err) => {
                eprintln!("{err}");
                return false;
            }
        };
        if self.is_readonly(&target_name) {
            self.readonly_write_error(&target_name);
            return false;
        }
        match &assignment.value {
            AssignmentValue::Array(elements) => {
                let entries = self.expand_array_entries(elements);
                if self.assoc.contains_key(&target_name) {
                    let map: BTreeMap<String, String> = assoc_entries(&entries).into_iter().collect();
                    if assignment.append {
                        self.assoc
                            .entry(target_name)
                            .or_default()
                            .extend(map);
                    } else {
                        self.assoc.insert(target_name, map);
                    }
                } else {
                    self.assign_array_entries(&target_name, &entries, assignment.append);
                }
            }
            AssignmentValue::Word(word) => {
                let value = self.expand_assignment(word);
                let index = assignment
                    .index
                    .as_ref()
                    .map(|index| self.expand_assignment(index))
                    .or(target_sub);
                if self.assoc.contains_key(&target_name) {
                    let key = index.unwrap_or_else(|| "0".to_string());
                    let map = self.assoc.entry(target_name).or_default();
                    let new_val = if assignment.append {
                        let mut prev = map.get(&key).cloned().unwrap_or_default();
                        prev.push_str(&value);
                        prev
                    } else {
                        value
                    };
                    map.insert(key, new_val);
                } else {
                    match index {
                        Some(index) => {
                            let new_val = if assignment.append {
                                let idx = self.resolve_array_index(&index);
                                let mut prev = self
                                    .arrays
                                    .get(&target_name)
                                    .and_then(|arr| arr.get(idx))
                                    .cloned()
                                    .flatten()
                                    .unwrap_or_default();
                                if self.int_vars.contains(&target_name) {
                                    let a = if prev.trim().is_empty() {
                                        0
                                    } else {
                                        arith::evaluate(&prev, self).unwrap_or(0)
                                    };
                                    let b = if value.trim().is_empty() {
                                        0
                                    } else {
                                        arith::evaluate(&value, self).unwrap_or(0)
                                    };
                                    a.wrapping_add(b).to_string()
                                } else {
                                    prev.push_str(&value);
                                    prev
                                }
                            } else {
                                value
                            };
                            self.set_array_element(&target_name, &index, new_val);
                        }
                        None => {
                            if assignment.append {
                                if self.int_vars.contains(&target_name) {
                                    let prev = self.var(&target_name);
                                    let a = if prev.trim().is_empty() {
                                        0
                                    } else {
                                        arith::evaluate(&prev, self).unwrap_or(0)
                                    };
                                    let b = if value.trim().is_empty() {
                                        0
                                    } else {
                                        arith::evaluate(&value, self).unwrap_or(0)
                                    };
                                    self.set_var(&target_name, a.wrapping_add(b).to_string());
                                } else {
                                    let mut prev = self.var(&target_name);
                                    prev.push_str(&value);
                                    self.set_var(&target_name, prev);
                                }
                            } else {
                                self.set_var(&target_name, value);
                            }
                        }
                    }
                }
            }
        }
        true
    }

    /// Expanded array literal elements, each carrying the expanded subscript
    /// text of its `[sub]=value` form, if any.
    fn expand_array_entries(&mut self, elements: &[Word]) -> Vec<(Option<String>, String)> {
        let mut entries = Vec::new();
        for element in elements {
            let (index, value) = match split_indexed_element(element) {
                Some((index, value)) => (Some(self.expand_assignment(&index)), value),
                None => (None, element.clone()),
            };
            for (position, value) in self.expand_array_element(&value).into_iter().enumerate() {
                let index = if position == 0 { index.clone() } else { None };
                entries.push((index, value));
            }
        }
        entries
    }

    fn expand_array_element(&mut self, element: &Word) -> Vec<String> {
        let expanded = self.expand_word(element);
        let empty_value = element.parts.is_empty()
            || (element.parts.len() == 1
                && matches!(&element.parts[0], Part::Double(inner) if inner.is_empty()));
        if expanded.is_empty() && empty_value {
            return vec![String::new()];
        }
        expanded
    }

    /// Store array literal entries: an element with a subscript lands on that
    /// index and the following plain elements continue after it.
    fn assign_array_entries(
        &mut self,
        name: &str,
        entries: &[(Option<String>, String)],
        append: bool,
    ) -> bool {
        if self.is_readonly(name) {
            self.readonly_write_error(name);
            return false;
        }
        let resolved: Vec<(Option<usize>, String)> = entries
            .iter()
            .map(|(index, value)| {
                (
                    index
                        .as_deref()
                        .map(|text| self.resolve_array_index(text)),
                    value.clone(),
                )
            })
            .collect();
        if !append {
            self.take_scalar(name);
            self.arrays.insert(name.to_string(), Vec::new());
        } else if !self.arrays.contains_key(name)
            && let Some(previous) = self.take_scalar(name)
        {
            self.arrays.insert(name.to_string(), vec![Some(previous)]);
        }
        let values = self.arrays.entry(name.to_string()).or_default();
        let mut next = values.len();
        for (index, value) in resolved {
            let position = index.unwrap_or(next);
            while values.len() <= position {
                values.push(None);
            }
            values[position] = Some(value);
            next = position + 1;
        }
        true
    }

    fn declaration(
        &mut self,
        command: &str,
        args: &[String],
        declarations: &[Assignment],
    ) -> i32 {
        let is_local_command = command == "local";
        if is_local_command && self.local_scopes.is_empty() {
            eprintln!("sharky: local: can only be used in a function");
            return 1;
        }

        let mut kind = None;
        let mut print = false;
        let mut list_functions = false;
        let mut only_indexed = false;
        let mut only_assoc = false;
        let mut attr_export: Option<bool> = None;
        let mut attr_nameref: Option<bool> = None;
        let mut attr_int: Option<bool> = None;
        let mut attr_lower: Option<bool> = None;
        let mut attr_upper: Option<bool> = None;
        let mut global = false;
        let readonly_flag = command == "readonly";
        let mut flag_readonly = false;
        let mut index = 0;
        while index < args.len() {
            let argument = args[index].as_str();
            if argument == "--" {
                index += 1;
                break;
            }
            if let Some(option) = argument.strip_prefix('-') {
                if option.is_empty() {
                    break;
                }
                for flag in option.chars() {
                    match flag {
                        'a' => {
                            kind = Some(false);
                            only_indexed = true;
                        }
                        'A' => {
                            kind = Some(true);
                            only_assoc = true;
                        }
                        'r' => flag_readonly = true,
                        'p' => print = true,
                        'x' => attr_export = Some(true),
                        'i' => attr_int = Some(true),
                        'l' => {
                            attr_lower = Some(true);
                            attr_upper = Some(false);
                        }
                        'u' => {
                            attr_upper = Some(true);
                            attr_lower = Some(false);
                        }
                        'n' if matches!(command, "declare" | "typeset" | "local") => {
                            attr_nameref = Some(true);
                        }
                        // Both flags are declare/typeset-only, as in bash.
                        'F' if matches!(command, "declare" | "typeset") => list_functions = true,
                        'g' if matches!(command, "declare" | "typeset") => global = true,
                        other => {
                            eprintln!("sharky: {command}: -{other}: unsupported option");
                            return 2;
                        }
                    }
                }
                index += 1;
                continue;
            }
            if let Some(option) = argument.strip_prefix('+') {
                if option.is_empty() {
                    break;
                }
                for flag in option.chars() {
                    match flag {
                        'x' => attr_export = Some(false),
                        'i' => attr_int = Some(false),
                        'l' => attr_lower = Some(false),
                        'u' => attr_upper = Some(false),
                        'n' if matches!(command, "declare" | "typeset" | "local") => {
                            attr_nameref = Some(false);
                        }
                        other => {
                            eprintln!("sharky: {command}: +{other}: unsupported option");
                            return 2;
                        }
                    }
                }
                index += 1;
                continue;
            }
            break;
        }
        let readonly = readonly_flag || flag_readonly;
        let export_flag = attr_export == Some(true);

        let names = &args[index..];
        if list_functions {
            if names.is_empty() {
                let mut function_names: Vec<&String> = self.funcs.keys().collect();
                function_names.sort();
                for name in function_names {
                    println!("declare -f {name}");
                }
                return 0;
            }
            let mut status = 0;
            for name in names {
                if self.funcs.contains_key(name.as_str()) {
                    println!("{name}");
                } else {
                    status = 1;
                }
            }
            return status;
        }

        // Declarations made by `declare`/`typeset` inside a function are local,
        // just like `local`; `-g` opts out and writes the global variable.
        let local = is_local_command
            || (matches!(command, "declare" | "typeset")
                && !global
                && !self.local_scopes.is_empty());
        if print {
            if is_local_command {
                return self.print_locals(names);
            }
            if command == "readonly" {
                return self.print_readonly(names);
            }
            if names.is_empty() {
                if export_flag {
                    self.print_exported();
                } else {
                    self.print_all_variables(only_indexed, only_assoc);
                }
                return 0;
            }
            let mut status = 0;
            for name in names {
                if !self.print_variable(name) {
                    eprintln!("sharky: {command}: {name}: not found");
                    status = 1;
                }
            }
            return status;
        }

        if names.is_empty() {
            if export_flag {
                self.print_exported();
                return 0;
            }
            // The bare `readonly` form is the same request as `readonly -p`.
            if command == "readonly" {
                return self.print_readonly(names);
            }
            eprintln!("sharky: {command}: usage: {command} [-a|-A] name[=value] ...");
            return 2;
        }

        let mut status = 0;
        for argument in &args[index..] {
            let (name, value) = match argument.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (argument.as_str(), None),
            };
            if !is_valid_array_name(name) {
                eprintln!("sharky: {command}: {name}: not a valid identifier");
                status = 1;
                continue;
            }

            let literal = declarations
                .iter()
                .find(|declaration| declaration.name == name);
            // A readonly name refuses every write, and `local` cannot shadow
            // one. A bare re-declaration of an already readonly name is
            // idempotent, the way bash treats `declare -r r` and `readonly r`.
            if self.is_readonly(name) {
                let writes = local || value.is_some() || literal.is_some() || kind.is_some();
                if writes {
                    if local || command == "declare" || command == "typeset" {
                        eprintln!("sharky: {command}: {name}: readonly variable");
                    } else {
                        self.readonly_write_error(name);
                    }
                    status = 1;
                }
                continue;
            }
            let fresh = local && self.save_local(name);
            if fresh {
                self.env.retain(|(key, _)| key != name);
                self.arrays.remove(name);
                self.assoc.remove(name);
                self.namerefs.remove(name);
                self.int_vars.remove(name);
                self.lower_vars.remove(name);
                self.upper_vars.remove(name);
            }

            // `declare -g` must reach the global binding even when a local
            // shadows the name. Bash expands the assignment words with the
            // local still visible, so expand the array literal first, then
            // reveal the saved global for the write and put it away
            // afterwards, leaving the local in place.
            let kind = kind.or(match literal.map(|declaration| &declaration.value) {
                Some(AssignmentValue::Array(_)) => Some(false),
                _ => None,
            });
            let literal_entries = match literal.map(|declaration| &declaration.value) {
                Some(AssignmentValue::Array(elements)) => Some(self.expand_array_entries(elements)),
                _ => None,
            };
            let literal_append = literal.is_some_and(|declaration| declaration.append);
            let mut global_swap = if global {
                self.reveal_global_binding(name)
            } else {
                None
            };

            if attr_nameref == Some(true) {
                let existing = self.take_scalar(name);
                self.arrays.remove(name);
                self.assoc.remove(name);
                self.namerefs.insert(name.to_string(), value.or(existing));
                match attr_export {
                    Some(true) => self.mark_exported(name),
                    Some(false) => self.clear_exported(name),
                    None => {}
                }
                if readonly {
                    self.mark_readonly(name);
                }
                self.finish_global_write(name, &mut global_swap);
                continue;
            }
            if attr_nameref == Some(false) {
                self.namerefs.remove(name);
            }

            match attr_int {
                Some(true) => {
                    self.int_vars.insert(name.to_string());
                }
                Some(false) => {
                    self.int_vars.remove(name);
                }
                None => {}
            }
            match attr_lower {
                Some(true) => {
                    self.lower_vars.insert(name.to_string());
                    self.upper_vars.remove(name);
                    if value.is_none()
                        && kind.is_none()
                        && let Some((_, existing)) =
                            self.env.iter_mut().rev().find(|(key, _)| key == name)
                    {
                        *existing = existing.to_lowercase();
                    }
                }
                Some(false) => {
                    self.lower_vars.remove(name);
                }
                None => {}
            }
            match attr_upper {
                Some(true) => {
                    self.upper_vars.insert(name.to_string());
                    self.lower_vars.remove(name);
                    if value.is_none()
                        && kind.is_none()
                        && let Some((_, existing)) =
                            self.env.iter_mut().rev().find(|(key, _)| key == name)
                    {
                        *existing = existing.to_uppercase();
                    }
                }
                Some(false) => {
                    self.upper_vars.remove(name);
                }
                None => {}
            }

            match kind {
                Some(true) => {
                    if self.arrays.contains_key(name) {
                        eprintln!(
                            "sharky: {command}: {name}: cannot convert indexed to associative array"
                        );
                        status = 1;
                        self.finish_global_write(name, &mut global_swap);
                        continue;
                    }
                    if local && !fresh {
                        self.env.retain(|(key, _)| key != name);
                    }
                    self.arrays.remove(name);
                    let scalar = self.take_scalar(name);
                    let map = self.assoc.entry(name.to_string()).or_default();
                    match literal_entries {
                        Some(entries) => {
                            if !literal_append {
                                map.clear();
                            }
                            for (key, value) in assoc_entries(&entries) {
                                map.insert(key, value);
                            }
                        }
                        None => {
                            if let Some(value) = value {
                                map.insert("0".to_string(), value);
                            } else if let Some(scalar) = scalar {
                                // A scalar becomes the element under key 0.
                                map.insert("0".to_string(), scalar);
                            }
                        }
                    }
                }
                Some(false) => {
                    if self.assoc.contains_key(name) {
                        eprintln!(
                            "sharky: {command}: {name}: cannot convert associative to indexed array"
                        );
                        status = 1;
                        self.finish_global_write(name, &mut global_swap);
                        continue;
                    }
                    if local && !fresh {
                        self.env.retain(|(key, _)| key != name);
                    }
                    self.assoc.remove(name);
                    let had_array = self.arrays.contains_key(name);
                    match literal_entries {
                        Some(entries) => {
                            if literal_append
                                && !had_array
                                && let Some(previous) = self.take_scalar(name)
                            {
                                self.arrays.insert(name.to_string(), vec![Some(previous)]);
                            }
                            self.assign_array_entries(name, &entries, literal_append);
                        }
                        None => {
                            if let Some(value) = value {
                                self.set_array_element(name, "0", value);
                            } else if !had_array {
                                match self.take_scalar(name) {
                                    Some(previous) => {
                                        self.arrays
                                            .insert(name.to_string(), vec![Some(previous)]);
                                    }
                                    None => {
                                        self.arrays.entry(name.to_string()).or_default();
                                    }
                                }
                            }
                        }
                    }
                }
                None => {
                    if let Some(value) = value {
                        self.set_var(name, value);
                    }
                }
            }
            match attr_export {
                Some(true) => self.mark_exported(name),
                Some(false) => self.clear_exported(name),
                None => {}
            }
            // The attribute is applied after the value, so `readonly r=1`
            // stores its value and only later writes are refused.
            if readonly {
                self.mark_readonly(name);
            }
            self.finish_global_write(name, &mut global_swap);
        }
        status
    }

    // Prints `declare -p` output for one name. Bash prints a named variable
    // whatever attribute flags were given, so filters only apply to the
    // no-name form below.
    fn print_variable(&self, name: &str) -> bool {
        match self.declaration_line(name) {
            Some(line) => {
                println!("{line}");
                true
            }
            None => false,
        }
    }

    // The no-name form lists every variable the shell holds, sorted by name.
    // -a and -A filter by array kind; bash prints nothing when both are given.
    fn print_all_variables(&self, only_indexed: bool, only_assoc: bool) {
        if only_indexed && only_assoc {
            return;
        }
        let mut names: Vec<&String> = self.env.iter().map(|(name, _)| name).collect();
        names.extend(self.arrays.keys());
        names.extend(self.assoc.keys());
        names.extend(self.namerefs.keys());
        names.extend(self.int_vars.iter());
        names.extend(self.lower_vars.iter());
        names.extend(self.upper_vars.iter());
        names.extend(self.exported.iter());
        names.extend(self.readonly_vars.iter());
        names.sort();
        names.dedup();
        for name in names {
            // `PIPESTATUS` is a special array bash does not list in
            // `declare -p` or `set`.
            if name == "PIPESTATUS" {
                continue;
            }
            let indexed = self.arrays.contains_key(name);
            let assoc = self.assoc.contains_key(name);
            if only_indexed && !indexed {
                continue;
            }
            if only_assoc && !assoc {
                continue;
            }
            if let Some(line) = self.declaration_line(name) {
                println!("{line}");
            }
        }
    }

    // `local -p` prints only the variables declared in the current function
    // frame. A name declared without a value still has a declaration to show.
    fn print_locals(&self, names: &[String]) -> i32 {
        let Some(frame) = self.local_scopes.last() else {
            return 1;
        };
        if names.is_empty() {
            let mut locals: Vec<&str> = frame.iter().map(|saved| saved.name.as_str()).collect();
            locals.sort();
            for name in locals {
                self.print_local(name);
            }
            return 0;
        }
        let mut status = 0;
        for name in names {
            if frame.iter().any(|saved| saved.name == *name) {
                self.print_local(name);
            } else {
                eprintln!("sharky: local: {name}: not found");
                status = 1;
            }
        }
        status
    }

    fn print_local(&self, name: &str) {
        match self.declaration_line(name) {
            Some(line) => println!("{line}"),
            None => println!("declare -- {name}"),
        }
    }

    // `readonly -p` and the bare `readonly` form list every readonly variable.
    // With a name bash prints nothing, whatever the name holds.
    fn print_readonly(&self, names: &[String]) -> i32 {
        if !names.is_empty() {
            return 0;
        }
        let mut vars: Vec<&String> = self.readonly_vars.iter().collect();
        vars.sort();
        for name in vars {
            match self.declaration_line(name) {
                Some(line) => println!("{line}"),
                None => println!("declare -r {name}"),
            }
        }
        0
    }

    fn var_attribute_flags(&self, name: &str) -> String {
        let mut flags = String::new();
        if self.arrays.contains_key(name) {
            flags.push('a');
        } else if self.assoc.contains_key(name) {
            flags.push('A');
        }
        if self.int_vars.contains(name) {
            flags.push('i');
        }
        if self.lower_vars.contains(name) {
            flags.push('l');
        }
        if self.namerefs.contains_key(name) {
            flags.push('n');
        }
        if self.readonly_vars.contains(name) {
            flags.push('r');
        }
        if self.upper_vars.contains(name) {
            flags.push('u');
        }
        if self.exported.contains(name) {
            flags.push('x');
        }
        flags
    }

    fn declaration_line(&self, name: &str) -> Option<String> {
        let attr = self.var_attribute_flags(name);
        if let Some(target_opt) = self.namerefs.get(name) {
            return Some(match target_opt {
                Some(target) => format!("declare -{attr} {name}={}", quote_value(target)),
                None => format!("declare -{attr} {name}"),
            });
        }
        if let Some(map) = self.assoc.get(name) {
            let mut line = format!("declare -{attr} {name}");
            if map.is_empty() {
                line.push_str("=()");
            } else {
                let mut keys: Vec<&String> = map.keys().collect();
                // Bash walks the 1024 bucket table in order, so equal buckets
                // fall back to the key order this shell already keeps.
                keys.sort_by_key(|key| bash_assoc_bucket(key));
                line.push_str("=(");
                for key in keys {
                    line.push('[');
                    line.push_str(&quote_assoc_key(key));
                    line.push_str("]=");
                    line.push_str(&quote_value(map.get(key).unwrap()));
                    line.push(' ');
                }
                line.push(')');
            }
            return Some(line);
        }
        if let Some(values) = self.arrays.get(name) {
            let mut line = format!("declare -{attr} {name}");
            // No "declared but unassigned" bit is kept, so a bare declaration
            // prints =() where bash prints only the name.
            if values.iter().all(Option::is_none) {
                line.push_str("=()");
            } else {
                line.push_str("=(");
                let mut first = true;
                for (index, value) in values.iter().enumerate() {
                    let Some(value) = value else {
                        continue;
                    };
                    if !first {
                        line.push(' ');
                    }
                    first = false;
                    line.push_str(&format!("[{index}]="));
                    line.push_str(&quote_value(value));
                }
                line.push(')');
            }
            return Some(line);
        }
        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| {
                let flags = if attr.is_empty() { "-" } else { attr.as_str() };
                format!("declare -{flags} {name}={}", quote_value(value))
            })
            .or_else(|| (!attr.is_empty()).then(|| format!("declare -{attr} {name}")))
    }

    fn save_local(&mut self, name: &str) -> bool {
        let already_saved = self
            .local_scopes
            .last()
            .is_some_and(|frame| frame.iter().any(|saved| saved.name == name));
        if already_saved {
            return false;
        }

        let var = self
            .env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone());
        let saved = SavedLocal {
            name: name.to_string(),
            var,
            array: self.arrays.get(name).cloned(),
            assoc: self.assoc.get(name).cloned(),
            exported: self.exported.contains(name),
            readonly: self.readonly_vars.contains(name),
            nameref: self.namerefs.get(name).cloned(),
            int_var: self.int_vars.contains(name),
            lower_var: self.lower_vars.contains(name),
            upper_var: self.upper_vars.contains(name),
        };
        if let Some(frame) = self.local_scopes.last_mut() {
            frame.push(saved);
        }
        true
    }

    fn restore_locals(&mut self) {
        let Some(frame) = self.local_scopes.pop() else {
            return;
        };
        for saved in frame.into_iter().rev() {
            self.take_scalar(&saved.name);
            self.arrays.remove(&saved.name);
            self.assoc.remove(&saved.name);
            if saved.exported {
                self.exported.insert(saved.name.clone());
            } else {
                self.exported.remove(&saved.name);
            }
            // The readonly flag is part of the binding: dropping the frame
            // drops a local one, and a shadowed global one comes back.
            if saved.readonly {
                self.readonly_vars.insert(saved.name.clone());
            } else {
                self.readonly_vars.remove(&saved.name);
            }
            if let Some(nameref) = saved.nameref {
                self.namerefs.insert(saved.name.clone(), nameref);
            } else {
                self.namerefs.remove(&saved.name);
            }
            if saved.int_var {
                self.int_vars.insert(saved.name.clone());
            } else {
                self.int_vars.remove(&saved.name);
            }
            if saved.lower_var {
                self.lower_vars.insert(saved.name.clone());
            } else {
                self.lower_vars.remove(&saved.name);
            }
            if saved.upper_var {
                self.upper_vars.insert(saved.name.clone());
            } else {
                self.upper_vars.remove(&saved.name);
            }
            if let Some(value) = saved.var {
                self.env.push((saved.name.clone(), value));
            }
            if let Some(values) = saved.array {
                self.arrays.insert(saved.name.clone(), values);
            }
            if let Some(values) = saved.assoc {
                self.assoc.insert(saved.name.clone(), values);
            }
        }
    }

    fn take_value(&mut self, name: &str) -> BindingValue {
        BindingValue {
            var: self.take_scalar(name),
            array: self.arrays.remove(name),
            assoc: self.assoc.remove(name),
        }
    }

    fn install_value(&mut self, name: &str, value: &BindingValue) {
        if let Some(var) = &value.var {
            self.env.push((name.to_string(), var.clone()));
        }
        if let Some(array) = &value.array {
            self.arrays.insert(name.to_string(), array.clone());
        }
        if let Some(assoc) = &value.assoc {
            self.assoc.insert(name.to_string(), assoc.clone());
        }
    }

    // The outermost frame that saved the name holds its global binding, which
    // a local shadows in the visible scope. Swap the two so a `declare -g`
    // write lands on the global; `restore_global_binding` swaps them back.
    fn reveal_global_binding(&mut self, name: &str) -> Option<GlobalSwap> {
        let frame = self
            .local_scopes
            .iter()
            .position(|frame| frame.iter().any(|saved| saved.name == name))?;
        let position = self.local_scopes[frame]
            .iter()
            .position(|saved| saved.name == name)?;
        let saved = self.local_scopes[frame].remove(position);
        let SavedLocal {
            var, array, assoc, ..
        } = saved;
        let local = self.take_value(name);
        self.install_value(name, &BindingValue { var, array, assoc });
        Some(GlobalSwap {
            frame,
            position,
            local,
        })
    }

    fn restore_global_binding(&mut self, name: &str, swap: GlobalSwap) {
        let value = self.take_value(name);
        self.install_value(name, &swap.local);
        // Attributes are name-wide in this shell, so the entry records the
        // ones the global carries now: `declare -gx`/`-gr` must survive the
        // frame's restore.
        self.local_scopes[swap.frame].insert(
            swap.position,
            SavedLocal {
                name: name.to_string(),
                var: value.var,
                array: value.array,
                assoc: value.assoc,
                exported: self.exported.contains(name),
                readonly: self.readonly_vars.contains(name),
                nameref: self.namerefs.get(name).cloned(),
                int_var: self.int_vars.contains(name),
                lower_var: self.lower_vars.contains(name),
                upper_var: self.upper_vars.contains(name),
            },
        );
    }

    fn finish_global_write(&mut self, name: &str, swap: &mut Option<GlobalSwap>) {
        if let Some(swap) = swap.take() {
            self.restore_global_binding(name, swap);
        }
    }

    fn set_array(&mut self, name: &str, values: Vec<String>, append: bool) -> bool {
        if self.is_readonly(name) {
            self.readonly_write_error(name);
            return false;
        }
        if !append {
            // Replacing the array also drops a scalar of the same name, which
            // bash no longer exports once the name is an array.
            self.take_scalar(name);
            self.arrays
                .insert(name.to_string(), values.into_iter().map(Some).collect());
            return true;
        }
        if !self.arrays.contains_key(name)
            && let Some(previous) = self.take_scalar(name)
        {
            self.arrays.insert(name.to_string(), vec![Some(previous)]);
        }
        self.arrays
            .entry(name.to_string())
            .or_default()
            .extend(values.into_iter().map(Some));
        true
    }

    fn resolve_array_index(&mut self, text: &str) -> usize {
        if let Ok(index) = text.trim().parse::<usize>() {
            return index;
        }
        arith::evaluate(text, self).unwrap_or(0).max(0) as usize
    }

    fn set_array_element(&mut self, name: &str, index: &str, mut value: String) -> bool {
        if self.is_readonly(name) {
            self.readonly_write_error(name);
            return false;
        }
        if self.int_vars.contains(name) {
            let n = if value.trim().is_empty() {
                0
            } else {
                arith::evaluate(&value, self).unwrap_or(0)
            };
            value = n.to_string();
        } else if self.lower_vars.contains(name) {
            value = value.to_lowercase();
        } else if self.upper_vars.contains(name) {
            value = value.to_uppercase();
        }
        if self.assoc.contains_key(name) {
            self.assoc
                .entry(name.to_string())
                .or_default()
                .insert(index.to_string(), value);
            return true;
        }
        // A subscript on a scalar name turns it into an array, keeping the old
        // value at index 0.
        if !self.arrays.contains_key(name)
            && !self.assoc.contains_key(name)
            && let Some(previous) = self.take_scalar(name)
        {
            self.arrays
                .insert(name.to_string(), vec![Some(previous)]);
        }
        let index = self.resolve_array_index(index);
        let array = self.arrays.entry(name.to_string()).or_default();
        while array.len() <= index {
            array.push(None);
        }
        array[index] = Some(value);
        true
    }

    pub(crate) fn set_var(&mut self, name: &str, value: String) -> bool {
        if matches!(self.namerefs.get(name), Some(None)) {
            if self.is_readonly(name) {
                self.readonly_write_error(name);
                return false;
            }
            self.namerefs.insert(name.to_string(), Some(value));
            return true;
        }
        let (target, sub) = match self.resolve_nameref(name) {
            Ok(resolved) => resolved,
            Err(err) => {
                eprintln!("{err}");
                return false;
            }
        };
        if let Some(sub) = sub {
            return self.set_array_element(&target, &sub, value);
        }
        if self.is_readonly(&target) {
            self.readonly_write_error(&target);
            return false;
        }
        self.set_var_unchecked(&target, value);
        true
    }

    /// The store write behind [`Shell::set_var`], without the readonly guard:
    /// the caller has already decided the write is allowed. A task's external
    /// environment is the one such caller, since `env` gives a command a plain
    /// variable even when the shell it came from marked the name readonly.
    fn set_var_unchecked(&mut self, name: &str, mut value: String) {
        if self.int_vars.contains(name) {
            let n = if value.trim().is_empty() {
                0
            } else {
                arith::evaluate(&value, self).unwrap_or(0)
            };
            value = n.to_string();
        } else if self.lower_vars.contains(name) {
            value = value.to_lowercase();
        } else if self.upper_vars.contains(name) {
            value = value.to_uppercase();
        }
        if name == "RANDOM" {
            let seed = bash_atoi(&value) as u32;
            self.random_state.set(seed);
            return;
        }
        if name == "SECONDS" {
            self.seconds_origin = std::time::Instant::now();
            self.seconds_offset = bash_atoi(&value);
            return;
        }
        if name == "LINENO" {
            self.lineno_offset = Some(bash_atoi(&value) - self.current_line as i64);
            return;
        }
        // Assigning OPTIND resets `getopts`, even when the value is unchanged.
        if name == "OPTIND" {
            self.getopts_dirty = true;
        }
        // `SHLVL` tracks its own numeric level so the no-fork final-command
        // path can export one less without changing the value the shell shows.
        if name == "SHLVL" {
            self.shlvl_level = bash_atoi(&value);
        }
        // A changed PATH can move every remembered external command.
        if name == "PATH" {
            self.command_cache.clear();
            self.highlight_cache.borrow_mut().clear();
        }
        // A name is either a scalar or an array. Writing a scalar to an array
        // name stores it in element 0, the way bash keeps the array type.
        if self.assoc.contains_key(name) {
            self.assoc
                .entry(name.to_string())
                .or_default()
                .insert("0".to_string(), value);
            return;
        }
        if self.arrays.contains_key(name) {
            self.set_array_element(name, "0", value);
            return;
        }
        match self.env.iter_mut().rev().find(|(key, _)| key == name) {
            Some(entry) => entry.1 = value,
            None => self.env.push((name.to_string(), value)),
        }
    }

    /// The environment handed to a child: only exported variables. `_` is not
    /// included here; `exec` replaces its value with the resolved command path.
    fn child_env(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .filter(|(name, _)| self.exported.contains(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    fn mark_exported(&mut self, name: &str) {
        // Bash never gives `_` the export attribute, even when asked.
        if name == "_" {
            return;
        }
        self.exported.insert(name.to_string());
    }

    pub(crate) fn is_readonly(&self, name: &str) -> bool {
        self.readonly_vars.contains(name) || matches!(name, "UID" | "EUID")
    }

    fn mark_readonly(&mut self, name: &str) {
        self.readonly_vars.insert(name.to_string());
    }

    // A refused write to a readonly name. Bash reports the plain form from a
    // plain assignment, `export`, `readonly` and from builtins that assign a
    // given name (`read`, `printf -v`); the declaration builtins add their own
    // name in front, which `declaration` prints itself.
    fn readonly_write_error(&self, name: &str) {
        eprintln!("sharky: {name}: readonly variable");
    }

    // A plain assignment statement that could not store its value ends the
    // input unit it runs on with status 1: bash discards the line it is
    // executing and reads the next one, and an interactive shell returns to its
    // prompt. With errexit set, the failed command leaves the shell instead.
    fn readonly_failure(&self) -> Flow {
        if !self.interactive && self.errexit {
            Flow::Exit(1)
        } else {
            Flow::Discard(1)
        }
    }

    fn clear_exported(&mut self, name: &str) {
        self.exported.remove(name);
    }

    // Removes a scalar binding so a name that became an array is no longer
    // exported with a stale value.
    fn take_scalar(&mut self, name: &str) -> Option<String> {
        let position = self.env.iter().rposition(|(key, _)| key == name)?;
        Some(self.env.remove(position).1)
    }

    // `export [-fn] [name[=value] ...]` and `export -p`. Bash keeps the export
    // attribute on the name, so a later plain assignment keeps it and a later
    // `export -n` drops it without touching the value.
    fn export(&mut self, args: &[String]) -> i32 {
        let mut clear = false;
        let mut print = false;
        let mut functions = false;
        let mut index = 0;
        while let Some(argument) = args.get(index) {
            if argument == "--" {
                index += 1;
                break;
            }
            if !argument.starts_with('-') || argument == "-" {
                break;
            }
            for flag in argument.chars().skip(1) {
                match flag {
                    'n' => clear = true,
                    'p' => print = true,
                    'f' => functions = true,
                    _ => {
                        eprintln!("sharky: export: -{flag}: invalid option");
                        eprintln!(
                            "sharky: export: usage: export [-fn] [name[=value] ...] or export -p [-f]"
                        );
                        return 2;
                    }
                }
            }
            index += 1;
        }

        let names = &args[index..];
        if names.is_empty() {
            if print || clear {
                self.print_exported();
                return 0;
            }
            return 0;
        }

        let mut status = 0;
        for argument in names {
            if functions {
                if !self.funcs.contains_key(argument) && !argument.contains('=') {
                    eprintln!("sharky: export: {argument}: not a function");
                    status = 1;
                }
                continue;
            }
            match argument.split_once('=') {
                Some((name, value)) => {
                    if !is_valid_array_name(name) {
                        eprintln!("sharky: export: `{name}': not a valid identifier");
                        status = 1;
                        continue;
                    }
                    // Bash still gives the export attribute to a name whose
                    // assignment it refused.
                    if !self.set_var(name, value.to_string()) {
                        status = 1;
                    }
                    if clear {
                        self.clear_exported(name);
                    } else {
                        self.mark_exported(name);
                    }
                }
                None => {
                    if !is_valid_array_name(argument) {
                        eprintln!("sharky: export: `{argument}': not a valid identifier");
                        status = 1;
                        continue;
                    }
                    if clear {
                        self.clear_exported(argument);
                    } else {
                        self.mark_exported(argument);
                    }
                }
            }
        }
        status
    }

    fn print_exported(&self) {
        let mut names: Vec<&String> = self.exported.iter().collect();
        names.sort();
        names.dedup();
        for name in names {
            match self.declaration_line(name) {
                Some(line) => println!("{line}"),
                None => println!("declare -x {name}"),
            }
        }
    }

    fn unset(&mut self, args: &[String]) -> i32 {
        let mut status = 0;
        let mut functions = false;
        let mut unset_nameref = false;
        let mut index = 0;
        while let Some(argument) = args.get(index) {
            if argument == "--" {
                index += 1;
                break;
            }
            if !argument.starts_with('-') || argument == "-" {
                break;
            }
            for flag in argument.chars().skip(1) {
                match flag {
                    'f' => functions = true,
                    'v' => {
                        functions = false;
                        unset_nameref = false;
                    }
                    'n' => {
                        functions = false;
                        unset_nameref = true;
                    }
                    other => {
                        eprintln!("sharky: unset: -{other}: invalid option");
                        eprintln!("sharky: unset: usage: unset [-f] [-v] [-n] [name ...]");
                        return 2;
                    }
                }
            }
            index += 1;
        }

        // `unset -f` targets functions only, so a readonly variable of the same
        // name is untouched.
        if functions {
            for name in &args[index..] {
                self.funcs.remove(name);
            }
            return 0;
        }

        if unset_nameref {
            for name in &args[index..] {
                if self.is_readonly(name) {
                    eprintln!("sharky: unset: {name}: cannot unset: readonly variable");
                    status = 1;
                    continue;
                }
                self.namerefs.remove(name);
                self.env.retain(|(key, _)| key != name);
                self.arrays.remove(name);
                self.assoc.remove(name);
                self.int_vars.remove(name);
                self.lower_vars.remove(name);
                self.upper_vars.remove(name);
                self.clear_exported(name);
            }
            return status;
        }

        for name in &args[index..] {
            let (base, mut elem_sub) = match split_array_argument(name) {
                Some((array, index)) => (array, Some(unquote_subscript(&index))),
                None => (name.clone(), None),
            };
            let (target, ref_sub) = match self.resolve_nameref(&base) {
                Ok(resolved) => resolved,
                Err(err) => {
                    eprintln!("{err}");
                    status = 1;
                    continue;
                }
            };
            if elem_sub.is_none() {
                elem_sub = ref_sub;
            }
            if let Some(sub) = elem_sub {
                if self.is_readonly(&target) {
                    eprintln!("sharky: unset: {target}: cannot unset: readonly variable");
                    status = 1;
                    continue;
                }
                if let Some(assoc) = self.assoc.get_mut(&target) {
                    assoc.remove(&sub);
                    continue;
                }
                if let Some(values) = self.arrays.get_mut(&target) {
                    if let Ok(idx) = sub.parse::<usize>()
                        && let Some(slot) = values.get_mut(idx)
                    {
                        *slot = None;
                    }
                    continue;
                }
            }
            if self.is_readonly(&target) {
                eprintln!("sharky: unset: {target}: cannot unset: readonly variable");
                status = 1;
                continue;
            }
            self.env.retain(|(key, _)| key != &target);
            self.arrays.remove(&target);
            self.assoc.remove(&target);
            self.namerefs.remove(&target);
            self.int_vars.remove(&target);
            self.lower_vars.remove(&target);
            self.upper_vars.remove(&target);
            self.clear_exported(&target);
            if target == "SHLVL" {
                self.shlvl_level = 0;
            }
        }
        status
    }

    pub fn reap_jobs(&mut self) {
        for line in self.collect_job_notices() {
            println!("{line}");
        }
    }

    /// Reaps the job table and returns the notices without printing them, so
    /// the line editor can place them above the input line it redraws. The
    /// prompt calls this when SIGCHLD reported a child and no command is
    /// running; the command boundary still uses `reap_jobs`.
    pub fn take_job_notices(&mut self) -> Vec<String> {
        self.collect_job_notices()
    }

    fn collect_job_notices(&mut self) -> Vec<String> {
        if self.jobs.is_empty() {
            return Vec::new();
        }

        // Bash marks the newest job `+` and the one before it `-`, whether or
        // not that job is in this batch of notices.
        let ids: Vec<usize> = self.jobs.iter().map(|job| job.id).collect();
        let newest = ids.iter().copied().max().unwrap_or(0);
        let previous = ids
            .iter()
            .copied()
            .filter(|id| *id != newest)
            .max()
            .unwrap_or(0);
        let marker = |id: usize| {
            if id == newest {
                '+'
            } else if id == previous {
                '-'
            } else {
                ' '
            }
        };

        let mut finished = Vec::new();
        let mut notices = Vec::new();
        for (index, job) in self.jobs.iter_mut().enumerate() {
            // Kept before the reap below empties `pids`: `wait` still has to
            // find the job by one of them after the job left the table.
            let all_pids = job.pids.clone();
            // A job owns every process in it: a pipeline whose first member
            // exited is still running when its last one has not. Reaping only
            // the leader would leave the others as zombies and the job forever
            // "Running". The last member's status is the one bash reports.
            let last = job.pids.len().saturating_sub(1);
            let mut alive = Vec::with_capacity(job.pids.len());
            let mut stopped = None;
            let mut continued = false;
            let mut ended: Option<(String, bool)> = None;
            let mut ended_status = None;
            for (position, pid) in job.pids.iter().enumerate() {
                match exec::wait_scan(*pid) {
                    // Already reaped elsewhere, or terminated now.
                    None => {}
                    Some(WaitStatus::Exited(_, code)) => {
                        if position == last {
                            ended = Some(exit_text(code));
                            ended_status = Some(code);
                        }
                    }
                    Some(WaitStatus::Signaled(_, signal, dumped)) => {
                        if position == last {
                            ended = Some((signal_text(signal as i32), dumped));
                            ended_status = Some(128 + signal as i32);
                        }
                    }
                    Some(WaitStatus::Stopped(_, signal)) => {
                        stopped.get_or_insert(signal as i32);
                        alive.push(*pid);
                    }
                    Some(WaitStatus::Continued(_)) => {
                        continued = true;
                        alive.push(*pid);
                    }
                    Some(_) => alive.push(*pid),
                }
            }
            job.pids = alive;
            // A job resumed by a signal from outside the shell is running
            // again, even though `kill -CONT` is not the builtin that did it.
            if continued && stopped.is_none() {
                job.stop = None;
            }
            if let Some(signal) = stopped
                && job.stop.is_none()
            {
                job.stop = Some(signal);
                // Bash separates a stop notice from the line above it.
                notices.push(String::new());
                notices.push(job_notice(job.id, marker(job.id), "Stopped", false, &job.command));
            }
            if job.pids.is_empty() {
                finished.push((index, job.id, all_pids, ended_status.unwrap_or(0)));
                if self.interactive {
                    let (status, dumped) = ended.unwrap_or_else(|| exit_text(0));
                    notices.push(job_notice(
                        job.id,
                        marker(job.id),
                        &status,
                        dumped,
                        &job.command,
                    ));
                }
            }
        }

        for (index, id, pids, status) in finished.into_iter().rev() {
            self.jobs.remove(index);
            self.retain_finished(id, &pids, status);
        }

        self.publish_jobs();
        notices
    }

    fn wait_foreground(&mut self, pids: &[Pid], pgid: Pid, command: String) -> i32 {
        let (status, _) = self.wait_job(pids, pgid, command, false);
        status
    }

    /// Publishes the job table's process groups for the hangup handler, which
    /// can only touch async-signal-safe state.
    fn publish_jobs(&self) {
        let groups: Vec<i32> = self
            .jobs
            .iter()
            .filter(|job| !job.no_hup)
            .map(|job| job.pgid.as_raw())
            .collect();
        crate::signal::publish_job_groups(&groups);
    }

    /// Adds a job that stopped, so `jobs`, `fg`, `bg` and `kill %n` can still
    /// reach it after the shell takes the terminal back.
    fn record_stopped_job(&mut self, pgid: Pid, command: String, signal: i32) {
        let id = self.next_job;
        self.next_job += 1;
        self.jobs.push(Job {
            id,
            pgid,
            pids: vec![pgid],
            command: command.clone(),
            stop: Some(signal),
            no_hup: false,
        });
        self.publish_jobs();
        println!(
            "\n{}",
            job_notice(id, '+', "Stopped", false, &command)
        );
    }

    fn wait_pipeline(&mut self, pids: &[Pid], pgid: Pid, command: String) -> i32 {
        let (status, statuses) = self.wait_job(pids, pgid, command, self.pipefail);
        if !statuses.is_empty() {
            self.set_pipe_status(&statuses);
        }
        status
    }

    fn wait_job(
        &mut self,
        pids: &[Pid],
        pgid: Pid,
        command: String,
        pipefail: bool,
    ) -> (i32, Vec<i32>) {
        let mut status = 0;
        let mut statuses = Vec::new();
        let mut stopped = None;

        for pid in pids {
            let code = match self.wait_foreground_member(*pid) {
                Some(WaitStatus::Exited(_, code)) => code,
                Some(WaitStatus::Signaled(_, signal, _)) => {
                    // A foreground command killed by Ctrl-C interrupts the
                    // input unit too, as bash aborts the whole construct the
                    // typed line was running.
                    if signal == Signal::SIGINT {
                        crate::signal::mark_interrupt();
                    }
                    128 + signal as i32
                }
                Some(WaitStatus::Stopped(_, signal)) => {
                    stopped = Some(signal as i32);
                    128 + signal as i32
                }
                // Its status was consumed elsewhere; it is not a failure.
                None => continue,
                _ => 0,
            };
            statuses.push(code);
            status = code;
        }

        // The foreground group is no longer running: cancel must not signal it.
        crate::agent::clear_foreground();

        if pipefail {
            status = statuses.iter().rev().find(|code| **code != 0).copied().unwrap_or(0);
        }

        self.reclaim_terminal();

        // The terminal is the shell's again and the job has finished: this is
        // the point where bash runs a trap that arrived while it waited.
        self.dispatch_traps();

        if stopped.is_some() && self.foreground_control() {
            let id = self.next_job;
            self.next_job += 1;
            self.jobs.push(Job {
                id,
                pgid,
                pids: pids.to_vec(),
                command: command.clone(),
                stop: stopped,
                no_hup: false,
            });
            self.publish_jobs();
            println!(
                "\n{}",
                job_notice(id, '+', "Stopped", false, &command)
            );
        }

        (status, statuses)
    }

    /// Publishes `PIPESTATUS`: the status of every stage of the foreground
    /// pipeline that just finished, in order.
    fn set_pipe_status(&mut self, statuses: &[i32]) {
        let values = statuses
            .iter()
            .map(|code| Some(code.to_string()))
            .collect();
        self.arrays.insert("PIPESTATUS".to_string(), values);
    }

    fn list_jobs(&self, args: &[String]) -> i32 {
        let mut pid_only = false;
        let mut long = false;
        let mut spec: Option<&str> = None;
        for arg in args {
            let Some(body) = arg.strip_prefix('-') else {
                spec = Some(arg);
                continue;
            };
            if body.is_empty() {
                spec = Some(arg);
                continue;
            }
            for flag in body.chars() {
                match flag {
                    'p' => pid_only = true,
                    'l' => long = true,
                    _ => {
                        eprintln!("sharky: jobs: -{flag}: invalid option");
                        eprintln!("sharky: jobs: usage: jobs [-lnp] [jobspec ...]");
                        return 2;
                    }
                }
            }
        }

        let selected = match spec {
            None => None,
            Some(text) => match self.job_index(Some(text)) {
                Some(index) => Some(index),
                None => {
                    eprintln!("sharky: jobs: {text}: no such job");
                    return 1;
                }
            },
        };

        let count = self.jobs.len();
        for (index, job) in self.jobs.iter().enumerate() {
            if selected.is_some_and(|selected| selected != index) {
                continue;
            }
            let marker = if index + 1 == count {
                '+'
            } else if index + 2 == count {
                '-'
            } else {
                ' '
            };
            if pid_only {
                println!("{}", job.pgid);
                continue;
            }
            let state = if job.stop.is_some() { "Stopped" } else { "Running" };
            if long {
                println!(
                    "[{}]{} {} {state:<24}{}",
                    job.id, marker, job.pgid, job.command
                );
            } else {
                println!("[{}]{}  {state:<24}{}", job.id, marker, job.command);
            }
        }
        0
    }

    fn job_index(&self, spec: Option<&str>) -> Option<usize> {
        let Some(spec) = spec else {
            return self.jobs.len().checked_sub(1);
        };
        let spec = spec.strip_prefix('%').unwrap_or(spec);
        if spec.is_empty() || spec == "+" {
            return self.jobs.len().checked_sub(1);
        }
        let id: usize = spec.parse().ok()?;
        self.jobs.iter().position(|job| job.id == id)
    }

    fn fg(&mut self, args: &[String]) -> i32 {
        let Some(index) = self.job_index(args.first().map(String::as_str)) else {
            eprintln!("sharky: fg: no such job");
            return 1;
        };

        let mut job = self.jobs.remove(index);
        job.stop = None;
        self.publish_jobs();
        println!("{}", job.command);

        self.set_foreground(job.pgid);
        let _ = killpg(job.pgid, Signal::SIGCONT);

        self.wait_foreground(&job.pids, job.pgid, job.command)
    }

    fn bg(&mut self, args: &[String]) -> i32 {
        let Some(index) = self.job_index(args.first().map(String::as_str)) else {
            eprintln!("sharky: bg: no such job");
            return 1;
        };

        let job = &mut self.jobs[index];
        job.stop = None;
        let _ = killpg(job.pgid, Signal::SIGCONT);
        println!("[{}]+ {} &", job.id, job.command);
        0
    }

    fn disown(&mut self, args: &[String]) -> i32 {
        let mut all = false;
        let mut no_hup = false;
        let mut running = false;
        let mut specs = Vec::new();
        let mut parsing_flags = true;
        for arg in args {
            if parsing_flags && arg == "--" {
                parsing_flags = false;
                continue;
            }
            let Some(body) = parsing_flags.then(|| arg.strip_prefix('-')).flatten() else {
                specs.push(arg.as_str());
                continue;
            };
            if body.is_empty() {
                specs.push(arg.as_str());
                continue;
            }
            for flag in body.chars() {
                match flag {
                    'a' => all = true,
                    'h' => no_hup = true,
                    'r' => running = true,
                    _ => {
                        eprintln!("sharky: disown: -{flag}: invalid option");
                        eprintln!("disown: usage: disown [-arh] [jobspec ...]");
                        return 2;
                    }
                }
            }
        }

        let mut indices = Vec::new();
        if all {
            indices.extend(0..self.jobs.len());
        } else if specs.is_empty() {
            if let Some(index) = self.jobs.len().checked_sub(1) {
                indices.push(index);
            }
        } else {
            for spec in specs {
                match self.job_index(Some(spec)) {
                    Some(index) => indices.push(index),
                    None => {
                        eprintln!("sharky: disown: {spec}: no such job");
                        return 1;
                    }
                }
            }
        }
        indices.sort_unstable();
        indices.dedup();
        if running {
            indices.retain(|&index| self.jobs[index].stop.is_none());
            if indices.is_empty() {
                eprintln!("sharky: disown: no running job");
                return 1;
            }
        }
        if no_hup {
            for index in indices {
                self.jobs[index].no_hup = true;
            }
        } else {
            for index in indices.into_iter().rev() {
                self.jobs.remove(index);
            }
        }
        self.publish_jobs();
        0
    }

    fn caller(&self, args: &[String]) -> i32 {
        let frame = match args.first() {
            None => None,
            Some(text) => match text.trim().parse::<usize>() {
                Ok(n) => Some(n),
                Err(_) => {
                    eprintln!("sharky: caller: {text}: invalid number");
                    return 2;
                }
            },
        };
        let idx = frame.unwrap_or(0);
        let Some(&line) = self.bash_lineno.get(idx) else {
            return 1;
        };
        let file = self
            .bash_source
            .get(idx + 1)
            .or_else(|| self.bash_source.get(idx))
            .map(String::as_str)
            .unwrap_or("NULL");
        if frame.is_some() {
            let func = self
                .funcname
                .get(idx + 1)
                .map(String::as_str)
                .unwrap_or("main");
            println!("{line} {func} {file}");
        } else {
            if self.funcname.is_empty() && self.source_depth == 0 {
                return 1;
            }
            println!("{line} {file}");
        }
        0
    }

    fn builtin_cmd(
        &mut self,
        args: &[String],
        declarations: &[Assignment],
        env: &[(String, String)],
    ) -> Flow {
        let mut index = 0;
        if args.get(index).is_some_and(|arg| arg == "--") {
            index += 1;
        }
        let rest = &args[index..];
        let Some(name) = rest.first() else {
            return Flow::Status(0);
        };
        if name == "exec" {
            return self.exec_builtin(&rest[1..], env);
        }
        match self.builtin(rest, declarations, env) {
            Some(flow) => flow,
            None => {
                eprintln!("sharky: builtin: {name}: not a shell builtin");
                Flow::Status(1)
            }
        }
    }

    fn source(&mut self, args: &[String]) -> Flow {
        let mut index = 0;
        if args.get(index).is_some_and(|arg| arg == "--") {
            index += 1;
        }
        let Some(raw_path) = args.get(index) else {
            eprintln!("sharky: source: filename argument required");
            return Flow::Status(2);
        };

        let resolved_path = if raw_path.contains('/') {
            PathBuf::from(raw_path)
        } else {
            let path_var = self.var("PATH");
            path_var
                .split(':')
                .filter(|dir| !dir.is_empty())
                .map(|dir| PathBuf::from(dir).join(raw_path))
                .find(|candidate| candidate.is_file())
                .unwrap_or_else(|| PathBuf::from(raw_path))
        };
        let display_path = resolved_path.to_string_lossy().into_owned();

        let source = match std::fs::read_to_string(&resolved_path) {
            Ok(source) => source,
            Err(err) => {
                eprintln!("sharky: {raw_path}: {err}");
                return Flow::Status(1);
            }
        };

        let program = match parser::parse(&source) {
            Ok(program) => program,
            Err(err) => {
                eprintln!("sharky: {}: {}", raw_path, err.message);
                return Flow::Status(2);
            }
        };

        let extra_args = &args[index + 1..];
        let saved = (!extra_args.is_empty())
            .then(|| std::mem::replace(&mut self.positional, extra_args.to_vec()));
        self.source_depth += 1;
        self.bash_source.insert(0, display_path);
        let pushed_source_fn = !self.funcname.is_empty();
        if pushed_source_fn {
            self.funcname.insert(0, "source".to_string());
        }
        self.bash_lineno.insert(0, self.current_line.max(1));
        self.sync_call_stack_arrays();

        let saved_line = self.current_line;
        let flow = self.run_program(&program);
        if matches!(flow, Flow::Status(_) | Flow::Return(_)) {
            let ret_status = match &flow {
                Flow::Return(code) => *code,
                Flow::Status(code) => *code,
                _ => self.last_status,
            };
            self.maybe_fire_return_trap(ret_status);
        }
        self.current_line = saved_line;

        if !self.bash_lineno.is_empty() {
            self.bash_lineno.remove(0);
        }
        if pushed_source_fn && !self.funcname.is_empty() {
            self.funcname.remove(0);
        }
        if !self.bash_source.is_empty() {
            self.bash_source.remove(0);
        }
        self.sync_call_stack_arrays();
        self.source_depth -= 1;
        if let Some(saved) = saved {
            self.positional = saved;
        }
        // A fatal error inside the file has already given up only the line it
        // ran on, so the rest of the file has run; the flow here is the file's
        // own last status. `return` leaves the sourced file, not the shell that
        // sourced it: the code becomes the command's status in the caller.
        match flow {
            Flow::Return(code) => Flow::Status(code),
            other => other,
        }
    }

    /// `test` and `[`: status 0 when true, 1 when false, 2 for a malformed
    /// expression, which is reported with the invoking name.
    fn test_builtin(&self, args: &[String], bracket: bool) -> i32 {
        match test_posix::run(args, bracket) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(message) => {
                let name = if bracket { "[" } else { "test" };
                eprintln!("sharky: {name}: {message}");
                2
            }
        }
    }

    /// `eval ARG ...`: join the arguments with spaces and run the result as
    /// shell code in the current shell, so assignments, functions and exports
    /// persist. Each complete statement runs as it is parsed, as in bash, so a
    /// syntax error later in the string leaves what preceded it in effect and
    /// reports status 2. The status is the last command's; `return`, `break`,
    /// `continue` and `exit` are handed back to the enclosing construct.
    fn eval_builtin(&mut self, args: &[String]) -> Flow {
        let index = match skip_leading_options("eval", "eval: usage: eval [arg ...]", args) {
            Ok(index) => index,
            Err(status) => return Flow::Status(status),
        };

        let source = args[index..].join(" ");
        match self.eval_units(&source) {
            Ok(flow) => flow,
            Err(err) => {
                if err.line > 0 {
                    eprintln!("sharky: eval: line {}: {}", err.line, err.message);
                } else {
                    eprintln!("sharky: eval: {}", err.message);
                }
                Flow::Status(2)
            }
        }
    }

    /// `umask [-p] [-S] [MODE]`: report or set the file-creation mask. The
    /// kernel mask is the source of truth, so children inherit it and files the
    /// shell creates are filtered by it.
    fn umask_builtin(&mut self, args: &[String]) -> i32 {
        let mut symbolic = false;
        let mut reusable = false;
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index += 1;
                break;
            }
            if arg == "--help" {
                if let Some(text) = crate::help::topic("umask") {
                    print!("{text}");
                }
                return 2;
            }
            if arg.len() > 1 && arg.starts_with('-') {
                for flag in arg[1..].chars() {
                    match flag {
                        'S' => symbolic = true,
                        'p' => reusable = true,
                        _ => {
                            eprintln!("sharky: umask: -{flag}: invalid option");
                            eprintln!("umask: usage: umask [-p] [-S] [mode]");
                            return 2;
                        }
                    }
                }
                index += 1;
                continue;
            }
            break;
        }

        let Some(word) = args.get(index) else {
            let current = umask_current();
            if reusable {
                print!("umask{} ", if symbolic { " -S" } else { "" });
            }
            if symbolic {
                println!("{}", umask_symbolic(current));
            } else {
                println!("{current:04o}");
            }
            return 0;
        };

        let bits = if word.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            match read_octal(word) {
                Some(bits) => bits,
                None => {
                    eprintln!("sharky: umask: {word}: octal number out of range");
                    return 1;
                }
            }
        } else {
            match umask_parse_symbolic(word, !umask_current() & 0o777) {
                // The parser works with the permitted bits; the mask is their
                // complement.
                Some(bits) => !bits & 0o777,
                None => return 1,
            }
        };
        umask_set(bits);
        if symbolic {
            println!("{}", umask_symbolic(bits));
        }
        0
    }

    /// `getopts OPTSTRING NAME [ARG ...]`: POSIX option parsing over ARG (or
    /// the positional parameters). State lives in OPTIND, OPTARG and OPTERR.
    fn getopts_builtin(&mut self, args: &[String]) -> i32 {
        let index = match skip_leading_options(
            "getopts",
            "getopts: usage: getopts optstring name [arg ...]",
            args,
        ) {
            Ok(index) => index,
            Err(status) => return status,
        };
        let args = &args[index..];
        let (Some(optstring), Some(name)) = (args.first(), args.get(1)) else {
            eprintln!("getopts: usage: getopts optstring name [arg ...]");
            return 2;
        };

        // A leading colon asks for silent reporting: no diagnostics, and the
        // offending option character is reported instead.
        let silent = optstring.starts_with(':');
        let options = if silent {
            &optstring[1..]
        } else {
            optstring.as_str()
        };
        let opterr = if silent {
            false
        } else {
            let value = self.var("OPTERR");
            value.is_empty() || atoi_like(&value) != 0
        };

        let scanning: Vec<String> = if args.len() > 2 {
            args[2..].to_vec()
        } else {
            self.positional.clone()
        };

        // A user assignment to OPTIND shows up as a value this shell did not
        // write, or as the assignment flag; OPTIND=1 (or 0, or a negative)
        // resets the scan, any other value moves the index.
        let current = self.var("OPTIND");
        if self.getopts_dirty || self.getopts_written.as_deref() != Some(current.as_str()) {
            self.getopts_dirty = false;
            let value = atoi_like(&current);
            let optind = if value < 0 || value == 1 {
                0
            } else {
                value as usize
            };
            self.getopts_optind = optind;
            if optind == 0 {
                self.getopts_pending = None;
            }
        }

        enum Scan {
            Char(u8),
            Eof,
            Invalid,
            Missing,
        }

        let argc = scanning.len() + 1;
        let mut optind = self.getopts_optind;
        let mut pending = self.getopts_pending;
        let mut outcome = Scan::Eof;
        let mut optarg: Option<String> = None;
        let mut optopt = b'?';

        if optind >= argc {
            optind = argc;
        } else {
            if optind == 0 {
                optind = 1;
                pending = None;
            }
            let mut decode = pending.is_some();
            if pending.is_none() && optind < argc {
                let word = &scanning[optind - 1];
                if word == "--" {
                    optind += 1;
                } else if word.starts_with('-') && word.len() > 1 {
                    pending = Some((optind, 1));
                    decode = true;
                }
            }
            if decode {
                let (curopt, charindex) = pending.expect("pending option word");
                // ARGS can change between calls; a word that no longer has the
                // half-scanned position is skipped instead of read past.
                let resumable = scanning
                    .get(curopt - 1)
                    .is_some_and(|word| charindex < word.len());
                if !resumable {
                    optind = (curopt + 1).min(argc);
                    pending = None;
                } else {
                    let mut charindex = charindex;
                    let element = scanning[curopt - 1].as_bytes();
                    let c = element[charindex];
                    charindex += 1;
                    optopt = c;
                    if charindex >= element.len() {
                        optind += 1;
                        pending = None;
                    } else {
                        pending = Some((curopt, charindex));
                    }

                    let position = options.as_bytes().iter().position(|&known| known == c);
                    let wants_argument =
                        position.is_some_and(|pos| options.as_bytes().get(pos + 1) == Some(&b':'));
                    if position.is_none() || c == b':' {
                        if opterr {
                            eprintln!("sharky: illegal option -- {}", c as char);
                        }
                        outcome = Scan::Invalid;
                    } else if wants_argument {
                        if pending.is_some() {
                            // The rest of the word is the option's argument.
                            optarg = Some(scanning[curopt - 1][charindex..].to_string());
                            optind += 1;
                            outcome = Scan::Char(c);
                        } else if optind == argc {
                            if opterr {
                                eprintln!("sharky: option requires an argument -- {}", c as char);
                            }
                            optopt = c;
                            optarg = Some(String::new());
                            outcome = Scan::Missing;
                        } else {
                            optarg = Some(scanning[optind - 1].clone());
                            optind += 1;
                            outcome = Scan::Char(c);
                        }
                        pending = None;
                    } else {
                        outcome = Scan::Char(c);
                    }
                }
            }
        }

        self.getopts_optind = optind;
        self.getopts_pending = pending;
        self.set_var("OPTIND", optind.to_string());
        self.getopts_written = Some(optind.to_string());
        self.getopts_dirty = false;

        match outcome {
            Scan::Eof => {
                self.unset_name("OPTARG");
                self.bind_getopts_name(name, "?");
                1
            }
            Scan::Invalid => {
                let status = self.bind_getopts_name(name, "?");
                if silent {
                    self.set_var("OPTARG", (optopt as char).to_string());
                } else {
                    self.unset_name("OPTARG");
                }
                status
            }
            Scan::Missing => {
                let status = self.bind_getopts_name(name, if silent { ":" } else { "?" });
                if silent {
                    self.set_var("OPTARG", (optopt as char).to_string());
                } else {
                    self.unset_name("OPTARG");
                }
                status
            }
            Scan::Char(c) => {
                match optarg {
                    Some(value) => {
                        self.set_var("OPTARG", value);
                    }
                    // bash leaves OPTARG bound to a null value, which every
                    // expansion treats as unset; remove it instead.
                    None => self.unset_name("OPTARG"),
                }
                self.bind_getopts_name(name, &(c as char).to_string())
            }
        }
    }

    fn unset_name(&mut self, name: &str) {
        if self.is_readonly(name) {
            self.readonly_write_error(name);
            return;
        }
        self.env.retain(|(key, _)| key != name);
        self.arrays.remove(name);
        self.assoc.remove(name);
        self.clear_exported(name);
        if name == "SHLVL" {
            self.shlvl_level = 0;
        }
        if name == "PATH" {
            self.command_cache.clear();
            self.highlight_cache.borrow_mut().clear();
        }
    }

    fn bind_getopts_name(&mut self, name: &str, value: &str) -> i32 {
        if !is_valid_array_name(name) {
            eprintln!("sharky: getopts: `{name}': not a valid identifier");
            return 1;
        }
        if !self.set_var(name, value.to_string()) {
            return 1;
        }
        0
    }

    /// Warns on a refused history file and falls back to memory only. The failed
    /// load is remembered so `history -c` cannot report clearing a file it never
    /// opened.
    pub fn prepare_history(&mut self, private: bool) {
        self.history_load_error = None;
        let limit = self.history_limit();
        self.history = Some(if private {
            History::private(limit)
        } else {
            match History::try_load(History::default_path(), limit) {
                Ok(history) => history,
                Err(err) => {
                    eprintln!("sharky: history: {err}");
                    self.history_load_error = Some(err);
                    History::private(limit)
                }
            }
        });
    }

    pub fn merge_history(&mut self) -> Result<(), String> {
        if let Some(err) = &self.history_load_error {
            return Err(err.clone());
        }
        let Some(history) = self.history.as_mut() else {
            return Err("history: not available".to_string());
        };
        history.merge()
    }

    /// Picks up history other sessions appended. The interactive loop calls this
    /// before each primary prompt only, so a continuation line keeps the view it
    /// started with; a warning is reported once, not on every prompt.
    pub fn refresh_history(&mut self) {
        if let Some(history) = self.history.as_mut() {
            if let Err(err) = history.refresh() {
                eprintln!("sharky: history: {err}");
            }
        }
    }

    pub fn push_history(&mut self, line: &str) {
        let cwd = self.cwd.to_string_lossy().into_owned();
        let paths = Shell::history_paths(line, &self.cwd);
        let named: Vec<&str> = paths.iter().map(String::as_str).collect();
        if let Some(history) = &mut self.history {
            if let Err(err) = history.try_add_with(line, Some(&cwd), &named) {
                eprintln!("sharky: {err}");
            }
        }
    }

    /// The words of `line` that name something that exists under `cwd` right now, at
    /// most [`history::MAX_PATHS`](crate::history::MAX_PATHS) of them.
    ///
    /// Recording only the paths that exist is what keeps the suggestion honest: a
    /// command whose output path does not exist yet names nothing and stays
    /// suggestible, while a later check can tell a candidate whose inputs are gone
    /// from one that is simply a repeat. Discovery happens once, at record time, so
    /// the editor never parses a command line.
    fn history_paths(line: &str, cwd: &Path) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        for word in line.split_whitespace() {
            if paths.len() == crate::history::MAX_PATHS {
                break;
            }
            let word = word.trim_matches(|c| c == '\'' || c == '"');
            if word.is_empty() || word.contains(|c: char| "|&;<>()$`\\".contains(c)) {
                continue;
            }
            if paths.iter().any(|seen| seen == word) {
                continue;
            }
            let candidate = Path::new(word);
            let resolved = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                cwd.join(candidate)
            };
            if resolved.exists() {
                paths.push(word.to_string());
            }
        }
        paths
    }

    pub fn history_entries(&self) -> &[String] {
        self.history
            .as_ref()
            .map(History::entries)
            .unwrap_or(&[])
    }

    pub fn history_entry_paths(&self, command: &str) -> (Option<&str>, &[String]) {
        self.history
            .as_ref()
            .map_or((None, &[]), |history| history.entry_meta(command))
    }

    /// The full in-memory view: each command once, at its newest position, with
    /// the newest timestamp known for it (absent for a legacy record) and where the
    /// session picked it up. The `history` builtin searches this; the suggest path
    /// reuses it. Order is oldest first, like [`Shell::history_entries`].
    pub fn history_records(&self) -> Vec<HistoryEntry> {
        self.history
            .as_ref()
            .map(History::records)
            .unwrap_or_default()
    }

    /// The entries `!!`, `!n` and `!prefix` may resolve: everything this session
    /// loaded at startup and ran itself, never a command a refresh picked up from
    /// another session. The editor and the `history` builtin use
    /// [`Shell::history_entries`] instead, so they still show everything.
    pub fn history_expansion_entries(&self) -> Vec<String> {
        self.history
            .as_ref()
            .map(History::expansion_entries)
            .unwrap_or_default()
    }

    fn z_builtin(&mut self, args: &[String], interactive: bool) -> i32 {
        if args.is_empty() {
            return self.cd(&[]);
        }
        if args.len() == 1 && args[0] == "-" {
            return self.cd(&["-".to_string()]);
        }

        if self.z.is_none() {
            self.z = Z::new();
        }
        let target = if interactive {
            self.z
                .as_ref()
                .and_then(|z| z.matches(args).into_iter().next())
        } else {
            self.z.as_ref().and_then(|z| z.query(args))
        };
        let Some(target) = target else {
            if args.is_empty() {
                eprintln!("sharky: z: no directories visited");
            } else {
                eprintln!("sharky: z: no match: {}", args.join(" "));
            }
            return 1;
        };
        self.cd(&["--".to_string(), target])
    }

    fn reload(&mut self) -> i32 {
        let Some(dir) = self.config_dir.clone() else {
            return 0;
        };
        self.aliases.clear();
        self.abbreviations.clear();
        self.keybinds.clear();
        self.settings.clear();
        self.colors.clear();
        self.styles.clear();
        self.highlight_cache.borrow_mut().clear();
        match config::load(&dir) {
            Ok(config) => {
                self.apply_config(config);
                self.discover_profile_paths();
                0
            }
            Err(err) => {
                eprintln!("sharky: config error in {err}");
                1
            }
        }
    }

    fn wait_builtin(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            let jobs: Vec<(usize, Vec<Pid>)> = self
                .jobs
                .iter()
                .map(|job| (job.id, job.pids.clone()))
                .collect();
            for (id, pids) in jobs {
                let known = self
                    .jobs
                    .iter()
                    .find(|job| job.id == id)
                    .and_then(|job| job.stop);
                let claimed = match known {
                    Some(signal) => Some(signal),
                    None => self.wait_group(&pids).1,
                };
                if let Some(signal) = claimed {
                    // Bash warns and keeps the job: it is not done yet, and
                    // fg/bg/kill still have to reach it.
                    if known.is_none()
                        && let Some(job) = self.jobs.iter_mut().find(|job| job.id == id)
                    {
                        job.stop = Some(signal);
                    }
                    eprintln!("sharky: wait: warning: job {id}[{}] stopped", pids[0]);
                    continue;
                }
                self.jobs.retain(|job| job.id != id);
            }
            self.publish_jobs();
            // A bare `wait` reports every job, so the retained statuses have
            // served their purpose. Bash keeps exactly its most recently
            // completed job answerable after this point; dropping all of them
            // is the simpler rule and is what the documented contract here
            // promises.
            self.finished_jobs.clear();
            for pid in std::mem::take(&mut self.substitution_pids) {
                let _ = exec::wait(pid);
            }
            return 0;
        }

        // Bash reads a dash argument as an option, so `wait -1` is an invalid
        // option and never reaches the job table: consuming that pid there
        // would steal the status a later `wait $p` has to report. Sharky
        // implements none of bash's wait options, so every one of them is a
        // clean error instead of a bogus pid lookup.
        if let Some(option) = args.iter().find(|arg| arg.starts_with('-')) {
            eprintln!("sharky: wait: {option}: invalid option");
            eprintln!("sharky: wait: usage: wait [id ...]");
            return 2;
        }

        // Bash waits for every listed id and reports the status of the last
        // one, so a failure for an earlier id does not hide a later success.
        let mut status = 0;
        for target in args {
            status = self.wait_target(target);
        }
        status
    }

    /// Waits for one `wait` argument: a `%job` spec, a pid, or a status kept
    /// after the job left the table.
    fn wait_target(&mut self, target: &str) -> i32 {
        if target.starts_with('%') {
            let Some(index) = self.job_index(Some(target)) else {
                if let Some(status) = self.finished_status(target) {
                    return status;
                }
                eprintln!("sharky: wait: {target}: no such process");
                return 127;
            };
            let id = self.jobs[index].id;
            let pids = self.jobs[index].pids.clone();
            return self.finish_wait(id, &pids);
        }

        let Ok(pid) = target.parse::<i32>() else {
            eprintln!("sharky: wait: {target}: no such process");
            return 127;
        };
        if let Some(index) = self
            .jobs
            .iter()
            .position(|job| job.pids.iter().any(|child| child.as_raw() == pid))
        {
            let id = self.jobs[index].id;
            let pids = self.jobs[index].pids.clone();
            return self.finish_wait(id, &pids);
        }
        if let Some(status) = self.finished_status(target) {
            return status;
        }

        match exec::wait_checked(Pid::from_raw(pid)) {
            Ok(status) => status,
            Err(Errno::ECHILD) => {
                eprintln!("sharky: wait: pid {pid}: is not a child of this shell");
                127
            }
            Err(err) => {
                eprintln!("sharky: wait: {err}");
                1
            }
        }
    }

    /// Remembers a job's status after it left the table, so a later `wait` by
    /// pid or `%id` still reports it the way bash does. The oldest entry goes
    /// once [`FINISHED_JOB_LIMIT`] entries are kept.
    fn retain_finished(&mut self, id: usize, pids: &[Pid], status: i32) {
        if self.finished_jobs.len() >= FINISHED_JOB_LIMIT {
            self.finished_jobs.remove(0);
        }
        self.finished_jobs.push(FinishedJob {
            id,
            pids: pids.to_vec(),
            status,
        });
    }

    /// The status kept for `target` (`%id` or a pid), if that job already left
    /// the table. Newest first, matching the most recent job with those pids.
    fn finished_status(&self, target: &str) -> Option<i32> {
        if let Some(spec) = target.strip_prefix('%') {
            let id: usize = spec.parse().ok()?;
            return self
                .finished_jobs
                .iter()
                .rev()
                .find(|job| job.id == id)
                .map(|job| job.status);
        }
        let pid: i32 = target.parse().ok()?;
        self.finished_jobs
            .iter()
            .rev()
            .find(|job| job.pids.iter().any(|child| child.as_raw() == pid))
            .map(|job| job.status)
    }

    /// Waits for one job and reports a stop the way bash does: the job stays
    /// in the table, because a stopped job is not finished and `fg`, `bg` or
    /// `kill %job` must still be able to reach it.
    fn finish_wait(&mut self, id: usize, pids: &[Pid]) -> i32 {
        // The stop status of a job that already reported itself stopped was
        // consumed by the notification path, so waiting again would block
        // forever. The recorded signal answers for it.
        if let Some(signal) = self
            .jobs
            .iter()
            .find(|job| job.id == id)
            .and_then(|job| job.stop)
        {
            eprintln!("sharky: warning: wait: job {id} is stopped");
            return 128 + signal;
        }

        let (status, stopped) = self.wait_group(pids);
        let Some(signal) = stopped else {
            // The status has to survive the job's departure: bash answers a
            // second `wait` for the same pid with it until a bare `wait` runs.
            self.retain_finished(id, pids, status);
            self.jobs.retain(|job| job.id != id);
            self.publish_jobs();
            return status;
        };
        if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
            job.stop = Some(signal);
        }
        self.publish_jobs();
        eprintln!("sharky: warning: wait: job {id} is stopped");
        128 + signal
    }

    /// Waits for every process of a job, returning 128+signal and that signal
    /// when one of them stops instead of exiting.
    fn wait_group(&self, pids: &[Pid]) -> (i32, Option<i32>) {
        let mut status = 0;
        for pid in pids {
            match exec::wait_stopped(*pid) {
                Some(WaitStatus::Exited(_, code)) => status = code,
                Some(WaitStatus::Signaled(_, signal, _)) => status = 128 + signal as i32,
                Some(WaitStatus::Stopped(_, signal)) => {
                    return (128 + signal as i32, Some(signal as i32));
                }
                // Already reaped; no status is left to report.
                None => {}
                _ => {}
            }
        }
        (status, None)
    }

    fn kill_builtin(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            eprintln!("sharky: kill: usage: kill [-SIGNAL] %job|pid ...");
            return 2;
        }

        let mut args = args;
        // `None` is signal 0, the existence check: it is not a signal the
        // `Signal` type can hold, but `kill -0` must still be accepted.
        let mut signal = Some(Signal::SIGTERM);
        if let Some(option) = args.first().and_then(|arg| arg.strip_prefix('-')) {
            if option == "0" {
                signal = None;
            } else {
                let Some(parsed) = parse_signal(option) else {
                    eprintln!("sharky: kill: invalid signal: {option}");
                    return 2;
                };
                signal = Some(parsed);
            }
            args = &args[1..];
        }

        if args.is_empty() {
            eprintln!("sharky: kill: usage: kill [-SIGNAL] %job|pid ...");
            return 2;
        }

        let mut status = 0;
        for target in args {
            if target.starts_with('%') {
                let Some(index) = self.job_index(Some(target.as_str())) else {
                    eprintln!("sharky: kill: {target}: no such job");
                    status = 1;
                    continue;
                };
                let pgid = self.jobs[index].pgid;
                let sent = match signal {
                    Some(signal) => killpg(pgid, signal).is_ok(),
                    // SAFETY: signal 0 only asks the kernel about the group.
                    None => unsafe { nix::libc::kill(-pgid.as_raw(), 0) == 0 },
                };
                if !sent {
                    eprintln!("sharky: kill: {target}: no such process");
                    status = 1;
                } else if let Some(signal) = signal {
                    // The job table follows the signal: a stopped job that is
                    // resumed is running again, and a running job that is
                    // stopped is not finished. Dropping the entry would orphan
                    // the process and lose `jobs`, `fg` and `wait`.
                    match signal {
                        Signal::SIGSTOP | Signal::SIGTSTP => {
                            self.jobs[index].stop = Some(signal as i32);
                        }
                        Signal::SIGCONT => self.jobs[index].stop = None,
                        _ => {}
                    }
                    self.publish_jobs();
                }
                continue;
            }

            let Ok(pid) = target.parse::<i32>() else {
                eprintln!("sharky: kill: {target}: no such process");
                status = 1;
                continue;
            };
            let sent = match signal {
                Some(signal) => nix::sys::signal::kill(Pid::from_raw(pid), signal).is_ok(),
                // SAFETY: signal 0 only asks the kernel about the process.
                None => unsafe { nix::libc::kill(pid, 0) == 0 },
            };
            if !sent {
                eprintln!("sharky: kill: {target}: no such process");
                status = 1;
            }
        }
        status
    }

    fn history_builtin(&mut self, args: &[String]) -> i32 {
        if self.history.is_none() {
            eprintln!("sharky: history: not available");
            return 1;
        }

        if args.is_empty() {
            for entry in self.history_entries() {
                println!("{entry}");
            }
            return 0;
        }
        if args.len() == 1 && args[0] == "-c" {
            if let Some(err) = &self.history_load_error {
                eprintln!("sharky: history: {err}");
                return 1;
            }
            let Some(history) = self.history.as_mut() else {
                return 1;
            };
            return match history.try_clear() {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("sharky: {err}");
                    1
                }
            };
        }
        if args.len() == 1 && args[0] == "merge" {
            return match self.merge_history() {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("sharky: {err}");
                    1
                }
            };
        }

        match self.history_query(args) {
            Ok(status) => status,
            Err(message) => {
                eprintln!("sharky: history: {message}");
                2
            }
        }
    }

    /// `history [search] [--max N] [--reverse] [--show-time] [--null]
    /// [--case-sensitive] [--contains|--prefix|--exact] [PATTERN]`, plus
    /// `history delete --exact PATTERN`.
    ///
    /// The default is a case-insensitive substring match, newest first, because the
    /// file's position order is the chronology this shell trusts; `when` is a hint
    /// and never reorders a listing. `--max 0` prints nothing, matching fish.
    fn history_query(&mut self, args: &[String]) -> Result<i32, String> {
        let mut options = HistoryOptions::default();
        let mut delete = false;
        let mut subcommand_seen = false;
        let mut pattern: Option<&str> = None;
        let mut index = 0;
        let mut options_done = false;
        while index < args.len() {
            let argument = args[index].as_str();
            index += 1;
            if !options_done && argument == "--" {
                options_done = true;
                continue;
            }
            if !options_done && argument.len() > 1 && argument.starts_with('-') {
                match argument {
                    "--max" => {
                        let value = args.get(index).ok_or("--max needs a count")?;
                        index += 1;
                        options.max = Some(
                            value
                                .parse::<usize>()
                                .map_err(|_| format!("--max: not a count: {value}"))?,
                        );
                    }
                    "--reverse" => options.reverse = true,
                    "--show-time" => options.show_time = true,
                    "--null" => options.null = true,
                    "--case-sensitive" => options.case_sensitive = true,
                    "--contains" => options.mode = MatchMode::Contains,
                    "--prefix" => options.mode = MatchMode::Prefix,
                    "--exact" => options.mode = MatchMode::Exact,
                    _ => return Err(format!("unknown option: {argument}")),
                }
                continue;
            }
            if !options_done && !subcommand_seen && pattern.is_none() {
                match argument {
                    "search" => {
                        subcommand_seen = true;
                        continue;
                    }
                    "delete" => {
                        delete = true;
                        subcommand_seen = true;
                        continue;
                    }
                    _ => {}
                }
            }
            if pattern.is_some() {
                return Err("too many patterns; quote the one you mean".to_string());
            }
            pattern = Some(argument);
        }
        let pattern = pattern.unwrap_or("");

        if delete {
            // Only the exact form is supported, as fish's builtin does: a
            // substring delete would need the interactive confirmation this
            // surface does not have.
            if options.mode != MatchMode::Exact {
                return Err("delete needs --exact".to_string());
            }
            // Exact delete is always case-sensitive, as fish's builtin is; the flag
            // is accepted so a script written for fish's requirement still works.
            if options.max.is_some() || options.reverse || options.show_time || options.null {
                return Err("delete accepts only --exact [--case-sensitive] PATTERN".to_string());
            }
            if pattern.is_empty() {
                return Err("delete needs a pattern".to_string());
            }
            let Some(history) = self.history.as_mut() else {
                return Ok(1);
            };
            return match history.try_delete_exact(pattern) {
                Ok(_) => Ok(0),
                Err(err) => Err(err),
            };
        }

        // Case folds once per entry, not once per comparison.
        let wanted = if options.case_sensitive {
            pattern.to_string()
        } else {
            pattern.to_lowercase()
        };
        let mut matches: Vec<HistoryEntry> = self
            .history_records()
            .into_iter()
            .rev()
            .filter(|entry| {
                let command = if options.case_sensitive {
                    entry.command.clone()
                } else {
                    entry.command.to_lowercase()
                };
                match options.mode {
                    MatchMode::Contains => command.contains(&wanted),
                    MatchMode::Prefix => command.starts_with(&wanted),
                    MatchMode::Exact => command == wanted,
                }
            })
            .collect();
        if let Some(max) = options.max {
            matches.truncate(max);
        }
        if options.reverse {
            matches.reverse();
        }

        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        for entry in &matches {
            if options.show_time {
                // A legacy record never carried a timestamp, so it prints no line
                // rather than a fabricated one.
                if let Some(when) = entry.when {
                    let _ = writeln!(out, "# {}", format_when(when));
                }
            }
            if options.null {
                let _ = write!(out, "{}\0", entry.command);
            } else {
                let _ = writeln!(out, "{}", entry.command);
            }
        }
        let _ = out.flush();
        Ok(0)
    }

    // Bash describes a name with the wording `command -V` uses, and the flags
    // pick a narrower view of the same precedence list. A function is followed
    // by its definition, in the layout `declare -f` uses.
    fn type_builtin(&mut self, args: &[String]) -> i32 {
        // Mirrors bash's describe_command flags: `short` starts set and each of
        // -p/-t/-P clears it, so the last of them decides the output mode.
        let mut all = false;
        let mut type_word = false;
        let mut path_only = false;
        let mut force = false;
        let mut short = true;
        let mut index = 0;
        while index < args.len() {
            let argument = args[index].as_str();
            if argument == "--" {
                index += 1;
                break;
            }
            let Some(option) = argument.strip_prefix('-') else {
                break;
            };
            if option.is_empty() {
                break;
            }
            for flag in option.chars() {
                match flag {
                    'a' => all = true,
                    'p' => {
                        path_only = true;
                        type_word = false;
                        short = false;
                    }
                    't' => {
                        type_word = true;
                        path_only = false;
                        short = false;
                    }
                    'P' => {
                        path_only = true;
                        force = true;
                        type_word = false;
                        short = false;
                    }
                    other => {
                        eprintln!("sharky: type: -{other}: invalid option");
                        eprintln!("sharky: type: usage: type [-afptP] name [name ...]");
                        return 2;
                    }
                }
            }
            index += 1;
        }

        let names = &args[index..];
        if names.is_empty() {
            return 0;
        }

        let mut status = 0;
        for name in names {
            let found = self.describe_name(name, all, type_word, path_only, force, short);
            if !found {
                if !path_only && !type_word {
                    eprintln!("sharky: type: {name}: not found");
                }
                status = 1;
            }
        }
        status
    }

    // The same alias > function > builtin > file precedence as `command -v`,
    // visited in order so `-a` can print every match. `force` skips the shell
    // definitions entirely, as `-P` requires.
    fn describe_name(
        &mut self,
        name: &str,
        all: bool,
        type_word: bool,
        path_only: bool,
        force: bool,
        short: bool,
    ) -> bool {
        let mut found = false;
        if !force {
            if let Some(value) = self.aliases.get(name) {
                let value = value.clone();
                if type_word {
                    println!("alias");
                } else if short {
                    println!("{name} is aliased to `{value}'");
                }
                found = true;
                if !all {
                    return true;
                }
            }
            if let Some(body) = self.funcs.get(name) {
                if type_word {
                    println!("function");
                } else if short {
                    println!("{name} is a function");
                    print!("{}", format_function_definition(name, body));
                }
                found = true;
                if !all {
                    return true;
                }
            }
            if is_builtin_name(name) {
                if type_word {
                    println!("builtin");
                } else if short {
                    println!("{name} is a shell builtin");
                }
                found = true;
                if !all {
                    return true;
                }
            }
        }
        if let Some(path) = exec::lookup(name, self) {
            if type_word {
                println!("file");
            } else if short {
                println!("{name} is {}", path.display());
            } else if path_only {
                println!("{}", path.display());
            }
            found = true;
        }
        found
    }

    // The one precedence rule: an alias shadows a function, which shadows a
    // builtin, which shadows an external command. `path` overrides the
    // environment PATH for the external lookup, which `command -p` needs.
    fn resolve_name(&self, name: &str, path: Option<&str>) -> Option<Resolution> {
        if let Some(value) = self.aliases.get(name) {
            Some(Resolution::Alias(value.clone()))
        } else if self.funcs.contains_key(name) {
            Some(Resolution::Function)
        } else if is_builtin_name(name) {
            Some(Resolution::Builtin)
        } else {
            let found = match path {
                Some(path) => exec::lookup_in(name, path),
                None => exec::lookup(name, self),
            };
            found.map(Resolution::External)
        }
    }

    // `command` runs a name without function lookup, and -v/-V only report how
    // the shell would resolve it. -p searches the default PATH instead of the
    // environment PATH, and it never changes what runs or its environment.
    // The last of -v and -V wins, so the two forms can be mixed in one word or
    // across words.
    fn command_builtin(&mut self, args: &[String], env: &[(String, String)]) -> Flow {
        let mut mode: Option<char> = None;
        let mut default_path = false;
        let mut index = 0;

        while let Some(argument) = args.get(index) {
            if argument == "--" {
                index += 1;
                break;
            }
            if !argument.starts_with('-') || argument == "-" {
                break;
            }
            for flag in argument.chars().skip(1) {
                match flag {
                    'v' | 'V' => mode = Some(flag),
                    'p' => default_path = true,
                    _ => {
                        eprintln!("sharky: command: -{flag}: invalid option");
                        eprintln!("sharky: command: usage: command [-pVv] command [arg ...]");
                        return Flow::Status(2);
                    }
                }
            }
            index += 1;
        }

        let words = &args[index..];

        if mode.is_some() {
            let verbose = mode == Some('V');
            let path = default_path.then_some(exec::DEFAULT_PATH);
            let mut found_any = false;
            for name in words {
                let Some(resolved) = self.resolve_name(name, path) else {
                    // Bash skips missing names here: -v stays silent, while -V
                    // reports each one on stderr. Neither changes the status on
                    // its own, which depends on whether any name resolved.
                    if verbose {
                        eprintln!("sharky: command: {name}: not found");
                    }
                    continue;
                };
                found_any = true;
                match resolved {
                    Resolution::Alias(value) if verbose => {
                        println!("{name} is aliased to `{value}'");
                    }
                    Resolution::Alias(value) => println!("alias {name}='{value}'"),
                    Resolution::Function if verbose => {
                        println!("{name} is a function");
                        if let Some(body) = self.funcs.get(name) {
                            print!("{}", format_function_definition(name, body));
                        }
                    }
                    Resolution::Function => println!("{name}"),
                    Resolution::Builtin if verbose => println!("{name} is a shell builtin"),
                    Resolution::Builtin => println!("{name}"),
                    Resolution::External(found) if verbose => {
                        println!("{name} is {}", found.display());
                    }
                    Resolution::External(found) => println!("{}", found.display()),
                }
            }
            // Bash reports 0 when at least one name resolves (and with no names
            // at all), and 1 only when every requested name is missing.
            let status = if found_any || words.is_empty() { 0 } else { 1 };
            return Flow::Status(status);
        }

        if words.is_empty() {
            return Flow::Status(0);
        }

        if let Some(flow) = self.builtin(words, &[], env) {
            return flow;
        }

        let resolved = match default_path {
            true => exec::resolve_in(&words[0], exec::DEFAULT_PATH),
            false => exec::resolve(&words[0], self),
        };
        let Some(program) = resolved else {
            eprintln!("sharky: command not found: {}", words[0]);
            if self.interactive
                && !self.session_mode
                && let Some(suggestion) = self.suggest_similar_command(&words[0])
            {
                eprintln!("sharky: did you mean '{suggestion}'?");
            }
            return Flow::Status(127);
        };

        let command = words.join(" ");
        let child = SpawnChild {
            program: Some(&program),
            ..SpawnChild::default()
        };
        match self.spawn(words, &[], env, child) {
            Ok(pid) => Flow::Status(self.wait_foreground(&[pid], pid, command)),
            Err(message) => {
                eprintln!("sharky: {message}");
                Flow::Status(1)
            }
        }
    }

    fn alias(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            let mut entries: Vec<_> = self.aliases.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            for (name, value) in entries {
                println!("alias {name}='{value}'");
            }
            return 0;
        }

        let mut status = 0;
        let mut index = 0;
        while index < args.len() {
            let argument = &args[index];
            let (name, value) = if let Some((name, value)) = argument.split_once('=') {
                index += 1;
                (name.to_string(), value.to_string())
            } else if index + 1 < args.len() {
                let pair = (argument.clone(), args[index + 1].clone());
                index += 2;
                pair
            } else {
                index += 1;
                match self.aliases.get(argument) {
                    Some(value) => {
                        println!("alias {argument}='{value}'");
                        continue;
                    }
                    None => {
                        eprintln!("sharky: alias: {argument}: not found");
                        status = 1;
                        continue;
                    }
                }
            };

            if name.is_empty() {
                eprintln!("sharky: alias: empty alias name");
                status = 1;
            } else if alias_has_control_operator(&value) {
                eprintln!("sharky: alias: {name}: control operator in alias value");
                status = 1;
            } else {
                self.aliases.insert(name, value);
            }
        }
        status
    }

    fn abbr(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            let mut entries: Vec<_> = self.abbreviations.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            for (name, value) in entries {
                println!("abbr {name}='{value}'");
            }
            return 0;
        }

        let mut status = 0;
        let mut index = 0;
        while index < args.len() {
            let argument = &args[index];
            let (name, value) = if let Some((name, value)) = argument.split_once('=') {
                index += 1;
                (name.to_string(), value.to_string())
            } else if index + 1 < args.len() {
                let pair = (argument.clone(), args[index + 1].clone());
                index += 2;
                pair
            } else {
                index += 1;
                match self.abbreviations.get(argument).cloned() {
                    Some(value) => {
                        println!("abbr {argument}='{value}'");
                        continue;
                    }
                    None => {
                        eprintln!("sharky: abbr: {argument}: not found");
                        status = 1;
                        continue;
                    }
                }
            };
            self.abbreviations.insert(name, value);
        }
        status
    }

    fn unabbr(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            eprintln!("sharky: unabbr: usage: unabbr [-a] name ...");
            return 1;
        }
        if args.len() == 1 && args[0] == "-a" {
            self.abbreviations.clear();
            return 0;
        }

        let mut status = 0;
        for name in args {
            if self.abbreviations.remove(name).is_none() {
                eprintln!("sharky: unabbr: {name}: not found");
                status = 1;
            }
        }
        status
    }

    fn unalias(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            eprintln!("sharky: unalias: usage: unalias [-a] name ...");
            return 1;
        }
        if args.len() == 1 && args[0] == "-a" {
            self.aliases.clear();
            return 0;
        }

        let mut status = 0;
        for name in args {
            if self.aliases.remove(name).is_none() {
                eprintln!("sharky: unalias: {name}: not found");
                status = 1;
            }
        }
        status
    }

    fn trap(&mut self, args: &[String]) -> i32 {
        let mut args = args;

        if let Some(first) = args.first() {
            if first == "-l" || first == "-L" {
                let names: Vec<&str> = crate::signal::NAMES
                    .iter()
                    .map(|(name, _)| *name)
                    .collect();
                println!("EXIT {}", names.join(" "));
                return 0;
            }
            if first == "-p" {
                args = &args[1..];
                if args.first().map(String::as_str) == Some("--") {
                    args = &args[1..];
                }
                if args.is_empty() {
                    self.print_traps(None);
                    return 0;
                }
                return self.print_traps(Some(args));
            }
            if first == "--" {
                args = &args[1..];
                if args.is_empty() {
                    self.print_traps(None);
                    return 0;
                }
            }
        }

        if args.is_empty() {
            self.print_traps(None);
            return 0;
        }

        if args[0] == "-" {
            if args.len() == 1 {
                eprintln!("sharky: trap: usage: trap [-Plp] [[action] signal_spec ...]");
                return 2;
            }
            let mut status = 0;
            for name in &args[1..] {
                match trap_signal(name) {
                    Some(signal) => self.clear_trap(signal),
                    None => {
                        eprintln!("sharky: trap: {name}: invalid signal specification");
                        status = 1;
                    }
                }
            }
            return status;
        }

        let action = args[0].clone();
        if args.len() == 1 {
            eprintln!("sharky: trap: usage: trap [-Plp] [[action] signal_spec ...]");
            return 2;
        }

        let mut status = 0;
        for name in &args[1..] {
            match trap_signal(name) {
                Some(signal) => self.set_trap(signal, &action),
                None => {
                    eprintln!("sharky: trap: {name}: invalid signal specification");
                    status = 1;
                }
            }
        }
        status
    }

    /// Stores an action and makes the signal reach it. `EXIT` has no
    /// disposition to change; `KILL` and `STOP` are stored but can never fire,
    /// which is what bash does with them.
    fn set_trap(&mut self, signal: &'static str, action: &str) {
        self.traps.insert(signal.to_string(), action.to_string());
        if signal == "EXIT" {
            // `restore_exit_trap` writes the session's inherited action behind
            // this builtin's back, so this flag alone says the action was
            // defined in this shell and is the one `finish` must run.
            self.exit_trap_defined_here = true;
        }
        let Some(number) = crate::signal::number(signal) else {
            return;
        };
        if crate::signal::cannot_catch(signal) {
            return;
        }
        if action.is_empty() {
            crate::signal::ignore(number);
        } else {
            crate::signal::catch(number);
        }
    }

    /// Removes an action and puts the signal back where it started: ignored for
    /// the line editor, terminal-restoring for the interactive fatal signals,
    /// default everywhere else.
    fn clear_trap(&mut self, signal: &'static str) {
        self.traps.remove(signal);
        let Some(number) = crate::signal::number(signal) else {
            return;
        };
        if crate::signal::cannot_catch(signal) {
            return;
        }
        self.restore_disposition(number);
    }

    fn restore_disposition(&self, number: i32) {
        if !self.interactive {
            // A batch shell keeps the notice handler for the signals that would
            // otherwise kill it outright, so an EXIT action still runs when an
            // untrapped TERM, HUP or INT ends the script, exactly as bash does
            // after `trap - TERM`.
            match number {
                nix::libc::SIGINT | nix::libc::SIGTERM | nix::libc::SIGHUP => {
                    crate::signal::fatal(number);
                }
                _ => crate::signal::reset(number),
            }
            return;
        }
        match number {
            nix::libc::SIGINT => crate::signal::interruptible(number),
            nix::libc::SIGQUIT
            | nix::libc::SIGTSTP
            | nix::libc::SIGTTIN
            | nix::libc::SIGTTOU => crate::signal::ignore(number),
            nix::libc::SIGTERM | nix::libc::SIGHUP => crate::signal::restore_terminal(number),
            nix::libc::SIGWINCH => crate::signal::watch_winch(),
            _ => crate::signal::reset(number),
        }
    }

    fn print_traps(&mut self, only: Option<&[String]>) -> i32 {
        let wanted: Option<Vec<&'static str>> = match only {
            None => None,
            Some(names) => {
                let mut wanted = Vec::new();
                for name in names {
                    match trap_signal(name) {
                        Some(signal) => wanted.push(signal),
                        None => {
                            eprintln!("sharky: trap: {name}: invalid signal specification");
                            return 1;
                        }
                    }
                }
                Some(wanted)
            }
        };

        for (signal, action) in self.installed_traps() {
            if let Some(wanted) = &wanted
                && !wanted.contains(&signal.as_str())
            {
                continue;
            }
            println!("trap -- '{action}' {}", trap_label(&signal));
        }
        0
    }

    /// Installed traps in the order bash prints them: `EXIT` first, then the
    /// signals by number.
    fn installed_traps(&self) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        if let Some(action) = self.traps.get("EXIT") {
            entries.push(("EXIT".to_string(), action.clone()));
        }
        if let Some(action) = self.traps.get("ERR") {
            entries.push(("ERR".to_string(), action.clone()));
        }
        if let Some(action) = self.traps.get("RETURN") {
            entries.push(("RETURN".to_string(), action.clone()));
        }
        let mut signals: Vec<&(&str, i32)> = crate::signal::NAMES.iter().collect();
        signals.sort_by_key(|(_, number)| *number);
        for (name, _) in signals {
            if let Some(action) = self.traps.get(*name) {
                entries.push((name.to_string(), action.clone()));
            }
        }
        entries
    }

    /// Runs the actions whose signals arrived since the last safe point. Called
    /// only between commands: never from a handler, never while a child owns
    /// the foreground process group.
    fn dispatch_traps(&mut self) {
        if self.trap_running || self.exit_requested.is_some() {
            return;
        }
        for number in crate::signal::take_pending() {
            let Some(name) = crate::signal::name(number) else {
                continue;
            };
            let Some(action) = self.traps.get(name).cloned() else {
                continue;
            };
            if action.trim().is_empty() {
                continue;
            }
            self.run_trap_action(&action);
            if self.trap_exit.is_some() {
                break;
            }
        }
    }

    fn run_trap_action(&mut self, action: &str) {
        // The action sees the status of the command it interrupted, and its own
        // status does not leak out: bash keeps `$?` from the interrupted command.
        let saved = self.last_status;
        self.trap_running = true;
        if let Err(err) = self.eval(action) {
            eprintln!("sharky: {}", err.message);
        }
        self.trap_running = false;
        if let Some(code) = self.exit_requested.take() {
            self.trap_exit = Some(code);
        }
        self.last_status = saved;
    }

    /// Dispatches what has arrived and reports an exit an action asked for, so
    /// the caller can unwind with the same flow a builtin `exit` produces.
    pub fn service_traps(&mut self) -> Option<i32> {
        self.dispatch_traps();
        let code = self.trap_exit.take();
        if code.is_some() {
            self.exit_requested = code;
        }
        code
    }

    /// Ctrl-C reaches the line editor as a byte, not a signal, because the
    /// terminal is in raw mode. Feed it back through the trap path so an
    /// `INT` action runs the way bash runs it, without touching the editor.
    pub fn interrupt_line(&mut self) -> Option<i32> {        if self
            .traps
            .get("INT")
            .is_some_and(|action| !action.trim().is_empty())
            && let Some(action) = self.traps.get("INT").cloned()
        {
            self.run_trap_action(&action);
        }
        let code = self.trap_exit.take();
        if code.is_some() {
            self.exit_requested = code;
        }
        code
    }

    /// A forked child keeps the trap list, which is why `( trap )` still prints
    /// the parent's actions, but bash puts every caught signal back to its
    /// default action there. Ignored signals stay ignored; pending deliveries
    /// belong to the parent.
    pub(crate) fn reset_child_traps(&mut self) {
        for (name, action) in &self.traps {
            if name == "EXIT" || action.is_empty() || crate::signal::cannot_catch(name) {
                continue;
            }
            if let Some(number) = crate::signal::number(name) {
                crate::signal::reset(number);
            }
        }
        crate::signal::clear_pending();
        crate::signal::clear_fatal();
        crate::signal::clear_child_pending();
        crate::signal::clear_winch_pending();
        self.trap_running = false;
        self.trap_exit = None;
        if !self.errtrace {
            self.traps.remove("ERR");
        }
        if !self.functrace {
            self.traps.remove("RETURN");
        }
        reset_child_signals();
        // The shell keeps fatal-signal handlers that write to the terminal. A
        // child shares the terminal's open file description, so that handler
        // must not run there: the default action is what a child gets. An
        // ignored signal keeps its disposition, which survives exec.
        for name in ["TERM", "HUP"] {
            if self.traps.get(name).is_some_and(|action| action.is_empty()) {
                continue;
            }
            if let Some(number) = crate::signal::number(name) {
                crate::signal::reset(number);
            }
        }
    }

    fn read(&mut self, args: &[String], env: &[(String, String)]) -> i32 {
        let (options, first_name) = match parse_read_options(args) {
            Ok(parsed) => parsed,
            Err(status) => return status,
        };
        let names = &args[first_name..];
        // A prefix assignment (`IFS=: read ...`) reaches the builtin through
        // `env`, so it must win over the shell's own value.
        let separators = env
            .iter()
            .find(|(key, _)| key == "IFS")
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| self.field_separators());
        if let Some(prompt) = &options.prompt
            && unsafe { nix::libc::isatty(0) } == 1
        {
            eprint!("{prompt}");
            let _ = std::io::stderr().flush();
        }

        // `-t 0` reports whether input is already available without consuming
        // any of it, which is how bash answers the same question.
        if options.timeout == Some(std::time::Duration::ZERO) {
            let mut descriptor = nix::libc::pollfd {
                fd: 0,
                events: nix::libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid descriptor and a zero timeout: poll only
            // inspects readiness and never blocks.
            let ready = unsafe { nix::libc::poll(&mut descriptor, 1, 0) } > 0;
            if !ready {
                return 1;
            }
            return self.assign_read(&options, names, &separators, "");
        }

        // `-n`/`-N` count Unicode scalar values; `-N` also stops ignoring the
        // delimiter, so it takes exactly that many characters.
        if let Some(limit) = options.exact_chars.or(options.chars) {
            if limit == 0 {
                return self.assign_read(&options, names, &separators, "");
            }
            let delimiter = if options.exact_chars.is_some() {
                None
            } else {
                Some(options.delimiter)
            };
            let (text, outcome) = read_stdin_chars(limit, delimiter, options.timeout);
            return match outcome {
                CharsOutcome::Interrupted => {
                    println!();
                    130
                }
                CharsOutcome::Timeout => {
                    let assign_status = self.assign_read(&options, names, &separators, &text);
                    if assign_status != 0 { assign_status } else { 142 }
                }
                CharsOutcome::Done => self.assign_read(&options, names, &separators, &text),
                CharsOutcome::End => {
                    let assign_status = self.assign_read(&options, names, &separators, &text);
                    if assign_status != 0 { assign_status } else { 1 }
                }
            };
        }

        let mut line = String::new();
        loop {
            match read_stdin_record(options.delimiter, options.timeout) {
                RecordInput::Record { text, terminated } => {
                    line.push_str(&text);
                    // A backslash-newline continues the line unless `-r` asked
                    // for raw input; the newline itself was already consumed.
                    if !options.raw && terminated && line.ends_with('\\') {
                        line.pop();
                        continue;
                    }
                    let assign_status = self.assign_read(&options, names, &separators, &line);
                    return if assign_status != 0 {
                        assign_status
                    } else if terminated {
                        0
                    } else {
                        1
                    };
                }
                RecordInput::Timeout(text) => {
                    line.push_str(&text);
                    let assign_status = self.assign_read(&options, names, &separators, &line);
                    return if assign_status != 0 { assign_status } else { 142 };
                }
                RecordInput::Interrupted => {
                    // A newline closes the line the terminal echoed, so the
                    // prompt starts clean, and the run loop sees 130.
                    println!();
                    return 130;
                }
            }
        }
    }

    /// Store a record the way the active `read` options ask: `-N` assigns the
    /// characters unchanged, `-a` fills an array, a bare `read` keeps the whole
    /// line in REPLY, and names split it.
    fn assign_read(
        &mut self,
        options: &ReadOptions,
        names: &[String],
        separators: &str,
        line: &str,
    ) -> i32 {
        if options.exact_chars.is_some() {
            let value = dequote_read_line(line, !options.raw);
            let name = names.first().map(String::as_str).unwrap_or("REPLY");
            return i32::from(!self.set_var(name, value));
        }
        if let Some(array) = &options.array {
            let fields = split_read_words(line, separators, !options.raw);
            if !self.set_array(array, fields, false) {
                return 1;
            }
            return 0;
        }
        if names.is_empty() {
            return i32::from(!self.set_var("REPLY", dequote_read_line(line, !options.raw)));
        }
        let fields = split_read_fields(line, separators, !options.raw, names.len());
        for (name, value) in names.iter().zip(fields) {
            if !self.set_var(name, value) {
                return 1;
            }
        }
        0
    }

    fn mapfile(&mut self, args: &[String]) -> i32 {
        let mut strip = false;
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            if arg == "--" {
                index += 1;
                break;
            }
            let Some(option) = arg.strip_prefix('-') else {
                break;
            };
            if option.is_empty() {
                break;
            }
            if option == "t" {
                strip = true;
                index += 1;
                continue;
            }
            eprintln!("sharky: mapfile: {arg}: invalid option");
            eprintln!("sharky: mapfile: usage: mapfile [-t] [array]");
            return 2;
        }

        let name = args
            .get(index)
            .cloned()
            .unwrap_or_else(|| "MAPFILE".to_string());
        if self.assoc.contains_key(&name) {
            eprintln!("sharky: mapfile: {name}: not an indexed array");
            return 1;
        }
        if !is_valid_array_name(&name) {
            eprintln!("sharky: mapfile: {name}: invalid identifier");
            return 1;
        }

        let mut values = Vec::new();
        loop {
            match read_stdin_line() {
                LineInput::Line(mut line) => {
                    if strip && line.ends_with('\n') {
                        line.pop();
                    }
                    values.push(line);
                }
                LineInput::Interrupted => {
                    println!();
                    return 130;
                }
                LineInput::End => break,
            }
        }
        if !self.set_array(&name, values, false) {
            return 1;
        }
        0
    }

    fn shift(&mut self, args: &[String]) -> i32 {
        let count = match args.first() {
            None => 1,
            Some(text) => match text.parse::<usize>() {
                Ok(count) => count,
                Err(_) => {
                    eprintln!("sharky: shift: {text}: numeric argument required");
                    return 1;
                }
            },
        };

        if count > self.positional.len() {
            return 1;
        }
        self.positional.drain(..count);
        0
    }

    fn active_options(&self) -> Vec<&'static str> {
        let mut active = Vec::new();
        if self.errexit {
            active.push("errexit");
        }
        if self.nounset {
            active.push("nounset");
        }
        if self.xtrace {
            active.push("xtrace");
        }
        if self.pipefail {
            active.push("pipefail");
        }
        if self.noclobber {
            active.push("noclobber");
        }
        if self.errtrace {
            active.push("errtrace");
        }
        if self.functrace {
            active.push("functrace");
        }
        if self.histexpand {
            active.push("histexpand");
        }
        if self.vi {
            active.push("vi");
        }
        active
    }

    fn shopt_value(&self, name: &str) -> Option<bool> {
        Some(match name {
            "globstar" => self.globstar,
            "autocd" => self.autocd_enabled(),
            "nullglob" => self.nullglob,
            "dotglob" => self.dotglob,
            "failglob" => self.failglob,
            "nocasematch" => self.nocasematch,
            "extglob" => self.extglob,
            _ => return None,
        })
    }

    fn set_shopt_option(&mut self, name: &str, on: bool) -> bool {
        match name {
            "globstar" => self.globstar = on,
            "autocd" => self.autocd = Some(on),
            "nullglob" => self.nullglob = on,
            "dotglob" => self.dotglob = on,
            "failglob" => self.failglob = on,
            "nocasematch" => self.nocasematch = on,
            "extglob" => self.extglob = on,
            _ => return false,
        }
        true
    }

    fn shopt(&mut self, args: &[String]) -> i32 {
        let mut set_mode: Option<bool> = None;
        let mut quiet = false;
        let mut print_reusable = false;
        let mut index = 0;

        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index += 1;
                break;
            }
            if !arg.starts_with('-') || arg == "-" {
                break;
            }
            for flag in arg.chars().skip(1) {
                match flag {
                    's' => set_mode = Some(true),
                    'u' => set_mode = Some(false),
                    'q' => quiet = true,
                    'p' => print_reusable = true,
                    other => {
                        eprintln!("sharky: shopt: -{other}: invalid option");
                        return 2;
                    }
                }
            }
            index += 1;
        }

        let names = &args[index..];
        if let Some(on) = set_mode {
            if names.is_empty() {
                for name in [
                    "autocd",
                    "dotglob",
                    "extglob",
                    "failglob",
                    "globstar",
                    "nocasematch",
                    "nullglob",
                ] {
                    if self.shopt_value(name) == Some(on) && !quiet {
                        if print_reusable {
                            println!("shopt {} {name}", if on { "-s" } else { "-u" });
                        } else {
                            println!("{name}\t{}", if on { "on" } else { "off" });
                        }
                    }
                }
                return 0;
            }
            let mut status = 0;
            for name in names {
                if !self.set_shopt_option(name, on) {
                    eprintln!("sharky: shopt: {name}: invalid shell option name");
                    status = 1;
                }
            }
            return status;
        }

        let targets: Vec<&str> = if names.is_empty() {
            vec![
                "autocd",
                "dotglob",
                "extglob",
                "failglob",
                "globstar",
                "nocasematch",
                "nullglob",
            ]
        } else {
            names.iter().map(String::as_str).collect()
        };

        let mut all_enabled = true;
        let mut status = 0;
        for name in targets {
            let Some(enabled) = self.shopt_value(name) else {
                eprintln!("sharky: shopt: {name}: invalid shell option name");
                status = 1;
                all_enabled = false;
                continue;
            };
            if !enabled {
                all_enabled = false;
            }
            if !quiet {
                if print_reusable {
                    println!("shopt {} {name}", if enabled { "-s" } else { "-u" });
                } else {
                    println!("{name}\t{}", if enabled { "on" } else { "off" });
                }
            }
        }
        if status != 0 {
            status
        } else if all_enabled {
            0
        } else {
            1
        }
    }

    fn set(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            let active = self.active_options();
            if !active.is_empty() {
                println!("{}", active.join(" "));
            }
            return 0;
        }

        let mut status = 0;
        let mut index = 0;
        while index < args.len() {
            let argument = args[index].as_str();
            if argument == "--" {
                self.positional = args[index + 1..].to_vec();
                return status;
            }

            let sign = match argument.chars().next() {
                Some(sign @ ('-' | '+')) => sign,
                _ => {
                    if let Some((key, value)) = argument.split_once('=') {
                        self.set_var(key, value.to_string());
                    } else {
                        eprintln!("sharky: set: {argument}: invalid option");
                        status = 1;
                    }
                    index += 1;
                    continue;
                }
            };
            let on = sign == '-';
            let letters = &argument[1..];
            if letters.is_empty() {
                eprintln!("sharky: set: {argument}: invalid option");
                status = 1;
                index += 1;
                continue;
            }

            for letter in letters.chars() {
                match letter {
                    'e' => self.errexit = on,
                    'u' => self.nounset = on,
                    'x' => self.xtrace = on,
                    'H' => self.histexpand = on,
                    'C' => self.noclobber = on,
                    'E' => self.errtrace = on,
                    'T' => self.functrace = on,
                    'o' => {
                        // Like bash, o consumes the next argument as the option
                        // name and any remaining cluster letters still apply.
                        index += 1;
                        match args.get(index).map(String::as_str) {
                            Some(name) => {
                                if !self.set_named_option(name, on) {
                                    eprintln!("sharky: set: {name}: invalid option name");
                                    status = 1;
                                }
                            }
                            None if on => self.list_options(),
                            None => self.list_option_commands(),
                        }
                    }
                    other => {
                        eprintln!("sharky: set: {sign}{other}: invalid option");
                        status = 1;
                    }
                }
            }
            index += 1;
        }
        status
    }

    pub fn set_short_option(&mut self, letter: char, on: bool) -> bool {
        match letter {
            'e' => self.errexit = on,
            'u' => self.nounset = on,
            'x' => self.xtrace = on,
            'H' => self.histexpand = on,
            'C' => self.noclobber = on,
            'E' => self.errtrace = on,
            'T' => self.functrace = on,
            _ => return false,
        }
        true
    }

    pub fn set_named_option(&mut self, name: &str, on: bool) -> bool {
        match name {
            "errexit" => self.errexit = on,
            "nounset" => self.nounset = on,
            "xtrace" => self.xtrace = on,
            "histexpand" => self.histexpand = on,
            "pipefail" => self.pipefail = on,
            "noclobber" => self.noclobber = on,
            "errtrace" => self.errtrace = on,
            "functrace" => self.functrace = on,
            "vi" => self.vi = on,
            _ => return false,
        }
        true
    }

    fn list_options(&self) {
        let active = self.active_options();
        if !active.is_empty() {
            println!("{}", active.join(" "));
        }
    }

    fn list_option_commands(&self) {
        for option in self.active_options() {
            println!("set -o {option}");
        }
    }

    fn cd(&mut self, args: &[String]) -> i32 {
        let mut physical = false;
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index += 1;
                break;
            }
            if !arg.starts_with('-') || arg == "-" {
                break;
            }
            for flag in arg.chars().skip(1) {
                match flag {
                    'L' => physical = false,
                    'P' => physical = true,
                    other => {
                        eprintln!("sharky: cd: -{other}: invalid option");
                        eprintln!("cd: usage: cd [-L|-P] [dir]");
                        return 2;
                    }
                }
            }
            index += 1;
        }
        let operands = &args[index..];
        if operands.len() > 1 {
            eprintln!("sharky: cd: too many arguments");
            return 1;
        }
        let mut print_dest = false;
        let target = match operands.first().map(String::as_str) {
            None | Some("") => {
                let home = self.var("HOME");
                if home.is_empty() {
                    eprintln!("sharky: cd: HOME not set");
                    return 1;
                }
                home
            }
            Some("-") => {
                let old = self.var("OLDPWD");
                if old.is_empty() {
                    eprintln!("sharky: cd: OLDPWD not set");
                    return 1;
                }
                print_dest = true;
                old
            }
            Some(path) => path.to_string(),
        };

        let previous = self.cwd.clone();
        let logical_candidate = if !physical {
            Some(normalize_logical_path(&self.cwd, Path::new(&target)))
        } else {
            None
        };
        let chdir_target = logical_candidate.as_deref().unwrap_or_else(|| Path::new(&target));
        match std::env::set_current_dir(chdir_target) {
            Ok(()) => {
                let actual = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
                self.cwd = if physical {
                    actual
                } else {
                    logical_candidate.unwrap_or(actual)
                };
                self.set_var("OLDPWD", previous.display().to_string());
                self.set_var("PWD", self.cwd.display().to_string());
                if print_dest {
                    println!("{}", self.cwd.display());
                }
                let cwd = self.cwd.display().to_string();
                if self.z.is_none() {
                    self.z = Z::new();
                }
                if let Some(z) = &mut self.z {
                    z.record(&cwd);
                }
                if self.cwd != previous {
                    self.record_dir_history();
                    self.run_hook("chpwd", &[]);
                }
                0
            }
            Err(err) => {
                eprintln!("sharky: cd: {target}: {err}");
                1
            }
        }
    }

    fn pwd(&self, args: &[String]) -> i32 {
        let mut physical = false;
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index += 1;
                break;
            }
            if !arg.starts_with('-') || arg == "-" {
                break;
            }
            for flag in arg.chars().skip(1) {
                match flag {
                    'L' => physical = false,
                    'P' => physical = true,
                    other => {
                        eprintln!("sharky: pwd: -{other}: invalid option");
                        return 2;
                    }
                }
            }
            index += 1;
        }
        let _ = index;
        if physical {
            let actual = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
            println!("{}", actual.display());
        } else {
            println!("{}", self.cwd.display());
        }
        0
    }

    /// Directory stack: PWD is the visible top, DIRSTACK holds the rest.
    /// Bash derives `dirs` from these two variables, so every mutation writes
    /// them back through the shell's own assignment path.
    fn dirstack(&self) -> Vec<String> {
        let mut stack = vec![self.var("PWD")];
        if let Some(values) = self.arrays.get("DIRSTACK") {
            stack.extend(values.iter().flatten().cloned());
        }
        stack.retain(|dir| !dir.is_empty());
        stack
    }

    fn write_dirstack(&mut self, stack: &[String]) {
        self.set_var("PWD", stack.first().cloned().unwrap_or_default());
        let tail: Vec<String> = stack.iter().skip(1).cloned().collect();
        self.set_array("DIRSTACK", tail, false);
    }

    fn change_to_stack_dir(&mut self, dir: &str) -> bool {
        let previous = self.cwd.clone();
        match std::env::set_current_dir(dir) {
            Ok(()) => {
                self.cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
                self.set_var("OLDPWD", previous.display().to_string());
                if self.cwd != previous {
                    self.record_dir_history();
                    self.run_hook("chpwd", &[]);
                }
                true
            }
            Err(err) => {
                eprintln!("sharky: {dir}: {err}");
                false
            }
        }
    }

    /// `pushd [-n] [+N | -N | DIR]`: rotate or push a directory and follow it.
    fn pushd(&mut self, args: &[String]) -> i32 {
        let mut stack = self.dirstack();
        if stack.is_empty() {
            eprintln!("sharky: pushd: directory stack empty");
            return 1;
        }
        let mut no_cd = false;
        let mut argument = None;
        for arg in args {
            match arg.as_str() {
                "-n" => no_cd = true,
                "+" | "-" => {
                    eprintln!("sharky: pushd: {arg}: invalid stack index");
                    return 2;
                }
                _ if argument.is_none() => argument = Some(arg.as_str()),
                _ => {
                    eprintln!("sharky: pushd: too many arguments");
                    return 1;
                }
            }
        }
        let head = stack[0].clone();
        let mut target = head.clone();
        match argument {
            None => {
                if stack.len() > 1 {
                    stack.swap(0, 1);
                }
                if !no_cd {
                    target = stack[0].clone();
                }
            }
            Some(spec) if spec.starts_with('+') || spec.starts_with('-') => {
                let Some(index) = parse_stack_index(spec) else {
                    eprintln!("sharky: pushd: {spec}: invalid number");
                    return 2;
                };
                let len = stack.len();
                if index >= len {
                    eprintln!("sharky: pushd: {spec}: directory stack index out of range");
                    return 1;
                }
                if spec.starts_with('+') {
                    stack.rotate_left(index);
                } else {
                    stack.rotate_left(len - 1 - index % len);
                }
                if !no_cd {
                    target = stack[0].clone();
                } else {
                    stack[0] = head;
                }
            }
            Some(dir) => {
                if no_cd {
                    stack.insert(1, dir.to_string());
                } else {
                    stack.insert(0, dir.to_string());
                    target = dir.to_string();
                }
            }
        }
        if !no_cd && !self.change_to_stack_dir(&target) {
            return 1;
        }
        self.write_dirstack(&stack);
        if !no_cd || matches!(argument, Some(dir) if !dir.starts_with('+') && !dir.starts_with('-')) {
            println!("{}", stack.join(" "));
        }
        0
    }

    /// `popd [-n] [+N | -N]`: leave the current directory and remove a stack entry.
    fn popd(&mut self, args: &[String]) -> i32 {
        let mut stack = self.dirstack();
        if stack.is_empty() {
            eprintln!("sharky: popd: directory stack empty");
            return 1;
        }
        let mut no_cd = false;
        let mut argument = None;
        for arg in args {
            match arg.as_str() {
                "-n" => no_cd = true,
                "+" | "-" => {
                    eprintln!("sharky: popd: {arg}: invalid stack index");
                    return 2;
                }
                _ if argument.is_none() => argument = Some(arg.as_str()),
                _ => {
                    eprintln!("sharky: popd: too many arguments");
                    return 1;
                }
            }
        }
        let index = match argument {
            None if no_cd => {
                // Without a numeric argument, `-n` leaves the top alone and
                // removes the next entry, the one pushd -n would have added.
                if stack.len() < 2 {
                    eprintln!("sharky: popd: directory stack empty");
                    return 1;
                }
                1
            }
            None => {
                if stack.len() == 1 {
                    eprintln!("sharky: popd: directory stack empty");
                    return 1;
                }
                0
            }
            Some(spec) if spec.starts_with('+') || spec.starts_with('-') => {
                let Some(spec_index) = parse_stack_index(spec) else {
                    eprintln!("sharky: popd: {spec}: invalid number");
                    return 2;
                };
                let len = stack.len();
                if spec_index >= len {
                    eprintln!("sharky: popd: {spec}: directory stack index out of range");
                    return 1;
                }
                let index = if spec.starts_with('+') {
                    spec_index
                } else {
                    // Negative indices count from the right, so -0 is the
                    // bottom entry, while +0 is the visible top.
                    len - 1 - spec_index
                };
                // With -n the current directory stays put, so bash never
                // removes the top entry: the lowest removable index is 1.
                if no_cd {
                    index.max(1)
                } else {
                    index
                }
            }
            Some(_) => {
                eprintln!("sharky: popd: too many arguments");
                return 1;
            }
        };
        let target = stack.get(1).cloned();
        stack.remove(index);
        if index == 0 && let Some(target) = target {
            self.change_to_stack_dir(&target);
        }
        self.write_dirstack(&stack);
        println!("{}", stack.join(" "));
        0
    }

    /// `dirs [-clpv]`: print the directory stack in the requested form.
    fn dirs(&mut self, args: &[String]) -> i32 {
        let mut clear = false;
        let mut print = false;
        let mut long = false;
        let mut numbered = false;
        for arg in args {
            let Some(body) = arg.strip_prefix('-') else {
                eprintln!("sharky: dirs: invalid argument: {arg}");
                return 2;
            };
            if body.is_empty() {
                continue;
            }
            for flag in body.chars() {
                match flag {
                    'c' => clear = true,
                    'p' => print = true,
                    'l' => long = true,
                    'v' => numbered = true,
                    _ => {
                        eprintln!("sharky: dirs: -{flag}: invalid option");
                        return 2;
                    }
                }
            }
        }
        if clear {
            self.set_array("DIRSTACK", Vec::new(), false);
            if !args.iter().any(|arg| arg != "-c") {
                return 0;
            }
        }
        let stack = self.dirstack();
        let display = |dir: &String| {
            if long {
                dir.clone()
            } else {
                match std::env::var("HOME") {
                    Ok(home)
                        if !home.is_empty()
                            && (dir == &home || dir.strip_prefix(&home).is_some_and(|rest| rest.starts_with('/')))=>
                    {
                        format!("~{}", &dir[home.len()..])
                    }
                    _ => dir.clone(),
                }
            }
        };
        for (index, dir) in stack.iter().enumerate() {
            let dir = display(dir);
            if numbered {
                println!("{index:>2}  {dir}");
            } else if print {
                println!("{dir}");
            } else if index == 0 {
                print!("{dir}");
            } else {
                print!(" {dir}");
            }
        }
        if !print && !numbered {
            println!();
        }
        0
    }

    /// `ulimit [-HS] [-cfnstuv] [LIMIT]`: show or set a process limit. The
    /// kernel is the source of truth, so the change reaches every child.
    fn ulimit(&self, args: &[String]) -> i32 {
        let mut hard = false;
        let mut soft = false;
        let mut all = false;
        let mut resource_flag: Option<char> = None;
        let mut value: Option<&str> = None;
        let mut positional = 0;
        for arg in args {
            if arg == "--" {
                continue;
            }
            // A bare negative number is the value, not an option cluster.
            if arg.starts_with("-")
                && arg.len() > 1
                && arg[1..].bytes().all(|byte| byte.is_ascii_digit())
            {
                positional += 1;
                if positional == 1 {
                    value = Some(arg.as_str());
                }
                continue;
            }
            if let Some(body) = arg.strip_prefix('-')
                && !body.is_empty()
                && body.chars().all(|c| c.is_ascii_alphabetic())
            {
                for flag in body.chars() {
                    match flag {
                        'H' => hard = true,
                        'S' => soft = true,
                        'a' => all = true,
                        'c' | 'f' | 'n' | 's' | 't' | 'u' | 'v' => {
                            resource_flag = Some(flag);
                        }
                        _ => {
                            eprintln!("sharky: ulimit: -{flag}: invalid option");
                            eprintln!("ulimit: usage: ulimit [-SHacfnstuv] [limit]");
                            return 2;
                        }
                    }
                }
                continue;
            }
            positional += 1;
            if positional == 1 {
                value = Some(arg.as_str());
            }
        }
        if positional > 1 || (all && value.is_some()) {
            eprintln!("sharky: ulimit: too many arguments");
            eprintln!("ulimit: usage: ulimit [-SHacfnstuv] [limit]");
            return 2;
        }

        let flag = resource_flag.unwrap_or('f');
        // Bash reports the soft limit when both -H and -S are given, but a
        // set with both flags writes the value to both limits.
        let use_hard = hard && !soft;
        let set_both = hard && soft;
        let kind = match flag {
            'c' => LimitKind::Blocks(Resource::RLIMIT_CORE),
            'f' => LimitKind::Blocks(Resource::RLIMIT_FSIZE),
            'n' => LimitKind::Count(Resource::RLIMIT_NOFILE),
            's' => LimitKind::Kbytes(Resource::RLIMIT_STACK),
            't' => LimitKind::Seconds(Resource::RLIMIT_CPU),
            'u' => LimitKind::Count(Resource::RLIMIT_NPROC),
            'v' => LimitKind::Kbytes(Resource::RLIMIT_AS),
            _ => unreachable!(),
        };

        if all {
            for (label, kind) in ALL_LIMITS {
                let (current, hard) = match getrlimit(kind.resource()) {
                    Ok(limits) => limits,
                    Err(err) => {
                        eprintln!("sharky: ulimit: {err}");
                        return 1;
                    }
                };
                let value = if use_hard { hard } else { current };
                println!("{label:<28} {}", kind.format(value));
            }
            return 0;
        }

        let (soft_current, hard_current) = match getrlimit(kind.resource()) {
            Ok(limits) => limits,
            Err(err) => {
                eprintln!("sharky: ulimit: {err}");
                return 1;
            }
        };
        let Some(text) = value else {
            let current = if use_hard { hard_current } else { soft_current };
            println!("{}", kind.format(current));
            return 0;
        };
        let raw = if text == "unlimited" {
            RLIM_INFINITY
        } else {
            let units = match text.parse::<u64>() {
                Ok(value) => value,
                Err(_) => {
                    eprintln!("sharky: ulimit: {text}: invalid number");
                    return 1;
                }
            };
            let multiplier = match kind {
                LimitKind::Blocks(_) => 512u64,
                LimitKind::Kbytes(_) => 1024u64,
                LimitKind::Count(_) | LimitKind::Seconds(_) => 1u64,
            };
            let Some(raw) = units.checked_mul(multiplier) else {
                eprintln!("sharky: ulimit: {text}: value too large");
                return 1;
            };
            raw as nix::sys::resource::rlim_t
        };
        let (new_soft, new_hard) = if set_both {
            (raw, raw)
        } else if use_hard {
            (soft_current.min(raw), raw)
        } else {
            (raw, hard_current)
        };
        if let Err(err) = setrlimit(kind.resource(), new_soft, new_hard) {
            eprintln!("sharky: ulimit: {}: cannot modify limit: {err}", kind.label());
            return 1;
        }
        0
    }

    /// `let` evaluates each arithmetic expression in sequence, updating shell
    /// variables as a side effect. Bash reports 0 when the last expression is
    /// nonzero, 1 when it is zero or an arithmetic error occurs.
    fn let_builtin(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            return 1;
        }
        let mut last = 0;
        for expression in args {
            match arith::evaluate(expression, self) {
                Ok(value) => last = value,
                Err(error) => {
                    if let arith::ArithError::Message(message) = error {
                        eprintln!("sharky: arithmetic: {message}");
                    }
                    return 1;
                }
            }
        }
        i32::from(last == 0)
    }

    /// Resolve a `~user`, `~+` or `~-` prefix, falling back to the literal text
    /// when the user (or the PWD/OLDPWD variable) is unavailable.
    fn expand_tilde_user(&self, name: &str) -> String {
        let home = match name {
            "+" => self.var("PWD"),
            "-" => self.var("OLDPWD"),
            other => match nix::unistd::User::from_name(other) {
                Ok(Some(user)) => return user.dir.to_string_lossy().into_owned(),
                _ => return format!("~{other}"),
            },
        };
        if home.is_empty() {
            format!("~{name}")
        } else {
            home
        }
    }

    fn expand_word(&mut self, word: &Word) -> Vec<String> {
        if let Some(alternatives) = brace_expand_word(word) {
            let mut fields = Vec::new();
            for alternative in alternatives {
                fields.extend(self.expand_word_no_brace(&alternative));
            }
            return fields;
        }
        self.expand_word_no_brace(word)
    }

    fn expand_word_no_brace(&mut self, word: &Word) -> Vec<String> {
        if let [Part::Literal(text)] = word.parts.as_slice()
            && !text.contains(QUOTED_GLOB_MARKER)
            && !has_glob(text)
        {
            return vec![text.clone()];
        }

        let mut fields = Fields::new(&self.field_separators());

        for part in &word.parts {
            match part {
                Part::Literal(text) => fields.literal(text),
                Part::Single(text) | Part::Escaped(text) => fields.push(text, true),
                Part::Tilde => {
                    let home = self.var("HOME");
                    fields.push(&home, true);
                }
                Part::TildeUser(name) => {
                    let home = self.expand_tilde_user(name);
                    fields.push(&home, true);
                }
                Part::Double(inner) => {
                    if inner.is_empty() {
                        fields.touch();
                        continue;
                    }
                    for part in inner {
                        match part {
                            Part::Variable(name) if name == "@" => {
                                fields.field_quoted_multi(&self.positional, None);
                            }
                            Part::Parameter(parameter) => {
                                if let Some(values) = self.parameter_elements(parameter) {
                                    let join = joins_elements(parameter)
                                        .then(|| self.ifs_join_separator());
                                    fields.field_quoted_multi(&values, join);
                                } else {
                                    let mut text = String::new();
                                    self.expand_parts(std::slice::from_ref(part), &mut text);
                                    fields.push(&text, true);
                                }
                            }
                            _ => {
                                let mut text = String::new();
                                self.expand_parts(std::slice::from_ref(part), &mut text);
                                fields.push(&text, true);
                            }
                        }
                    }
                }
                Part::Variable(name) if name == "@" => {
                    fields.field_multi(&self.positional, None);
                }
                Part::Variable(name) if name == "*" => {
                    let value = self.var("*");
                    fields.push(&value, false);
                }
                Part::Variable(name) => {
                    let value = self.expansion_var(name);
                    fields.push(&value, false);
                }
                Part::ProcessSub(source, input) => {
                    let path = self.process_substitution(source, *input);
                    fields.push(&path, true);
                }
                Part::Parameter(parameter) => {
                    if let Some(values) = self.parameter_elements(parameter) {
                        let join = self.unquoted_join(parameter);
                        fields.field_multi(&values, join);
                    } else {
                        let value = self.parameter_value(parameter);
                        fields.push(&value, false);
                    }
                }
                Part::CmdSub(source) => {
                    let value = self.command_substitution(source);
                    fields.push(&value, false);
                }
                Part::Arithmetic(source) => match arith::evaluate(source, self) {
                    Ok(value) => fields.push(&value.to_string(), false),
                    Err(arith::ArithError::Readonly) => {
                        self.expansion_error = true;
                        fields.push("0", false);
                    }
                    Err(arith::ArithError::Message(message)) => {
                        eprintln!("sharky: arithmetic: {message}");
                        self.expansion_error = true;
                        fields.push("0", false);
                    }
                },
            }
        }

        self.glob_fields(fields.finish())
    }

    fn expand_pattern_word(&mut self, word: &Word) -> String {
        let mut out = String::new();
        for part in &word.parts {
            match part {
                Part::Literal(text) => out.push_str(text),
                Part::Single(text) | Part::Escaped(text) => quote_pattern(&mut out, text),
                Part::Tilde => {
                    let home = self.var("HOME");
                    quote_pattern(&mut out, &home);
                }
                Part::TildeUser(name) => {
                    let home = self.expand_tilde_user(name);
                    quote_pattern(&mut out, &home);
                }
                Part::Double(inner) => {
                    let mut text = String::new();
                    self.expand_parts(inner, &mut text);
                    quote_pattern(&mut out, &text);
                }
                Part::ProcessSub(source, input) => {
                    let path = self.process_substitution(source, *input);
                    quote_pattern(&mut out, &path);
                }
                other => {
                    let mut text = String::new();
                    self.expand_parts(std::slice::from_ref(other), &mut text);
                    out.push_str(&text);
                }
            }
        }
        out
    }

    fn expand_text_word(&mut self, word: &Word) -> String {
        let mut text = String::new();
        self.expand_parts(&word.parts, &mut text);
        text
    }

    fn expand_assignment(&mut self, word: &Word) -> String {
        self.expand_text_word(word)
    }

    fn expand_parts(&mut self, parts: &[Part], out: &mut String) {
        self.expand_parts_in(parts, out, false);
    }

    fn expand_parts_in(&mut self, parts: &[Part], out: &mut String, quoted: bool) {
        for part in parts {
            match part {
                Part::Literal(text) | Part::Single(text) | Part::Escaped(text) => out.push_str(text),
                Part::Double(inner) => self.expand_parts_in(inner, out, true),
                Part::Variable(name) => {
                    let value = self.expansion_var(name);
                    out.push_str(&value);
                }
                Part::Parameter(parameter) => {
                    let value = if quoted {
                        self.parameter_value_quoted(parameter)
                    } else {
                        self.parameter_value(parameter)
                    };
                    out.push_str(&value);
                }
                Part::ProcessSub(source, input) => {
                    out.push_str(&self.process_substitution(source, *input))
                }
                Part::CmdSub(source) => out.push_str(&self.command_substitution(source)),
                Part::Arithmetic(source) => match arith::evaluate(source, self) {
                    Ok(value) => out.push_str(&value.to_string()),
                    Err(arith::ArithError::Readonly) => self.expansion_error = true,
                    Err(arith::ArithError::Message(message)) => {
                        eprintln!("sharky: arithmetic: {message}");
                        self.expansion_error = true;
                    }
                },
                Part::Tilde => out.push_str(&self.var("HOME")),
                Part::TildeUser(name) => out.push_str(&self.expand_tilde_user(name)),
            }
        }
    }

    fn expand_string(&mut self, text: &str) -> String {
        let Ok(tokens) = lexer::tokenize(text) else {
            return text.to_string();
        };

        // A parameter word is one word even when it contains unquoted blanks, so
        // the text between two tokens is copied verbatim instead of being
        // re-joined with a single space.
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        for token in &tokens {
            let TokenKind::Word(word) = &token.kind else {
                continue;
            };
            let mut gap = token.pos;
            while gap > 0 && chars.get(gap - 1).is_some_and(|char| char.is_whitespace()) {
                gap -= 1;
            }
            out.extend(chars.get(gap..token.pos).unwrap_or_default().iter());
            out.push_str(&self.expand_text_word(word));
        }
        out
    }

    fn fail_indirect(&mut self, name: &str, message: &str) {
        eprintln!("sharky: {name}: {message}");
        self.expansion_error = true;
    }

    fn fail_parameter_expansion(&mut self, name: &str, argument: &str) {
        let word = self.expand_string(argument);
        let message = if word.is_empty() {
            "parameter null or not set".to_string()
        } else {
            word
        };
        eprintln!("sharky: {name}: {message}");
        self.expansion_error = true;
        self.null_parameter_error = true;
    }

    /// The keys selected by `${!a[@]}` or `${!a[*]}`. Outside double quotes the
    /// at form joins with a space; inside them it keeps the first IFS character.
    /// With a null IFS the at form falls back to a space and the star form joins
    /// with nothing.
    fn join_parameter_keys(&mut self, parameter: &Parameter, quoted: bool) -> String {
        let at_form = parameter.index.as_deref() == Some("@");
        let separator = match self.ifs_first_char() {
            Some(separator) if quoted || !at_form => separator.to_string(),
            Some(_) => " ".to_string(),
            None if at_form => " ".to_string(),
            None => String::new(),
        };
        let target_name = match self.resolve_nameref(&parameter.name) {
            Ok((target, _)) => target,
            Err(_) => return String::new(),
        };
        if let Some(array) = self.arrays.get(&target_name) {
            return array
                .iter()
                .enumerate()
                .filter(|(_, value)| value.is_some())
                .map(|(index, _)| index.to_string())
                .collect::<Vec<_>>()
                .join(&separator);
        }
        if let Some(assoc) = self.assoc.get(&target_name) {
            return assoc.keys().cloned().collect::<Vec<_>>().join(&separator);
        }
        self.scalar_key(&target_name).unwrap_or_default()
    }

    /// A defined scalar behaves like a one-element array whose only key is 0.
    /// An undefined name has no keys at all.
    fn scalar_key(&self, name: &str) -> Option<String> {
        self.env
            .iter()
            .any(|(key, _)| key == name)
            .then(|| "0".to_string())
    }

    /// A parameter expansion that appears inside double quotes in a context
    /// without word splitting. Only the key list tells the two contexts apart.
    fn parameter_value_quoted(&mut self, parameter: &Parameter) -> String {
        if parameter.operator == Some(ParamOp::Keys)
            && matches!(parameter.index.as_deref(), Some("@") | Some("*"))
        {
            return self.join_parameter_keys(parameter, true);
        }
        self.parameter_value(parameter)
    }

    fn parameter_value(&mut self, parameter: &Parameter) -> String {
        let inspect_nameref_attr = parameter.index.is_none()
            && matches!(
                (parameter.operator, parameter.argument.as_deref()),
                (Some(ParamOp::Transform), Some("a") | Some("A"))
            )
            && self.namerefs.contains_key(&parameter.name);
        let (target_name, nameref_sub) = if inspect_nameref_attr {
            (parameter.name.clone(), None)
        } else {
            match self.resolve_nameref(&parameter.name) {
                Ok(resolved) => resolved,
                Err(err) => {
                    eprintln!("{err}");
                    self.expansion_error = true;
                    return String::new();
                }
            }
        };
        let index = parameter.index.as_deref().or(nameref_sub.as_deref());
        let multiple = matches!(index, Some("@") | Some("*"));

        if parameter.operator == Some(ParamOp::Length) && multiple {
            if let Some(array) = self.arrays.get(&target_name) {
                return array.iter().flatten().count().to_string();
            }
            if let Some(assoc) = self.assoc.get(&target_name) {
                return assoc.len().to_string();
            }
            return "0".to_string();
        }

        if parameter.operator == Some(ParamOp::Keys) && multiple {
            return self.join_parameter_keys(parameter, false);
        }

        let expanded = match index {
            Some(key) if key != "@" && key != "*" => Some(self.expand_string(key)),
            _ => None,
        };
        let lookup = expanded.as_deref().or(index);

        let numeric_index = if self.assoc.contains_key(&target_name) {
            0
        } else {
            lookup
                .map(|text| self.resolve_array_index(text))
                .unwrap_or(0)
        };

        let no_nounset = matches!(
            parameter.operator,
            Some(
                ParamOp::Default
                    | ParamOp::Assign
                    | ParamOp::Alternate
                    | ParamOp::Error
                    | ParamOp::DefaultUnset
                    | ParamOp::AssignUnset
                    | ParamOp::AlternateUnset
                    | ParamOp::ErrorUnset
                    // The indirect form reports its own failure for an unset
                    // name, which bash keeps at status 1.
                    | ParamOp::Indirect
            )
        ) || inspect_nameref_attr;
        // Without word splitting the at form joins its elements with a space,
        // whatever IFS holds; the star form keeps the first IFS character.
        let multi_separator = if lookup == Some("@") {
            " ".to_string()
        } else {
            self.ifs_join_separator()
        };
        let value = if inspect_nameref_attr {
            String::new()
        } else if let Some(assoc) = self.assoc.get(&target_name) {
            match lookup {
                Some("@") | Some("*") => assoc
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(&multi_separator),
                Some(key) => assoc.get(key).cloned().unwrap_or_default(),
                None => assoc.get("0").cloned().unwrap_or_default(),
            }
        } else if let Some(array) = self.arrays.get(&target_name) {
            match lookup {
                Some("@") | Some("*") => array
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(&multi_separator),
                Some(_) => array
                    .get(numeric_index)
                    .cloned()
                    .flatten()
                    .unwrap_or_default(),
                None => array.first().cloned().flatten().unwrap_or_default(),
            }
        } else if no_nounset {
            self.var(&target_name)
        } else {
            self.expansion_var(&target_name)
        };
        let argument = parameter.argument.clone().unwrap_or_default();
        let is_numeric = !target_name.is_empty()
            && target_name.chars().all(|char| char.is_ascii_digit());
        let defined = if inspect_nameref_attr {
            true
        } else if self.namerefs.get(&target_name).is_some_and(Option::is_none) {
            false
        } else if let Some(assoc) = self.assoc.get(&target_name) {
            match lookup {
                Some("@") | Some("*") => !assoc.is_empty(),
                Some(key) => assoc.contains_key(key),
                None => assoc.contains_key("0"),
            }
        } else if let Some(array) = self.arrays.get(&target_name) {
            match lookup {
                Some("@") | Some("*") => array.iter().any(Option::is_some),
                Some(_) => array.get(numeric_index).is_some_and(Option::is_some),
                None => array.first().is_some_and(Option::is_some),
            }
        } else if target_name == "0" {
            true
        } else if is_numeric {
            target_name
                .parse::<usize>()
                .ok()
                .and_then(|index| index.checked_sub(1))
                .is_some_and(|index| index < self.positional.len())
        } else if target_name == "@" || target_name == "*" {
            !self.positional.is_empty()
        } else if is_special_name(&target_name) {
            true
        } else {
            self.env.iter().any(|(key, _)| key == &target_name)
        };

        match parameter.operator {
            None => value,
            Some(ParamOp::Length) => value.chars().count().to_string(),
            Some(ParamOp::Default) => {
                if value.is_empty() {
                    self.expand_string(&argument)
                } else {
                    value
                }
            }
            Some(ParamOp::DefaultUnset) => {
                if defined {
                    value
                } else {
                    self.expand_string(&argument)
                }
            }
            Some(ParamOp::Assign) => {
                if value.is_empty() {
                    let new = self.expand_string(&argument);
                    if !self.set_var(&parameter.name, new.clone()) {
                        // A refused write expands to nothing and abandons the
                        // input unit, the same failure as `${name:?}`.
                        self.expansion_error = true;
                        return String::new();
                    }
                    new
                } else {
                    value
                }
            }
            Some(ParamOp::AssignUnset) => {
                if defined {
                    value
                } else {
                    let new = self.expand_string(&argument);
                    if !self.set_var(&parameter.name, new.clone()) {
                        self.expansion_error = true;
                        return String::new();
                    }
                    new
                }
            }
            Some(ParamOp::Alternate) => {
                if value.is_empty() {
                    String::new()
                } else {
                    self.expand_string(&argument)
                }
            }
            Some(ParamOp::AlternateUnset) => {
                if defined {
                    self.expand_string(&argument)
                } else {
                    String::new()
                }
            }
            Some(ParamOp::Error) => {
                if value.is_empty() {
                    self.fail_parameter_expansion(&parameter.name, &argument);
                }
                value
            }
            Some(ParamOp::ErrorUnset) => {
                if !defined {
                    self.fail_parameter_expansion(&parameter.name, &argument);
                }
                value
            }
            Some(ParamOp::TrimPrefixShort) => {
                trim_prefix_opts(&value, &argument, false, self.extglob, self.nocasematch)
            }
            Some(ParamOp::TrimPrefixLong) => {
                trim_prefix_opts(&value, &argument, true, self.extglob, self.nocasematch)
            }
            Some(ParamOp::TrimSuffixShort) => {
                trim_suffix_opts(&value, &argument, false, self.extglob, self.nocasematch)
            }
            Some(ParamOp::TrimSuffixLong) => {
                trim_suffix_opts(&value, &argument, true, self.extglob, self.nocasematch)
            }
            Some(ParamOp::Substring) => match self.slice_parts(&argument) {
                Some((offset, length)) => {
                    let chars: Vec<char> = value.chars().collect();
                    bound_slice(chars.len(), offset, length)
                        .map(|(start, end)| chars[start..end].iter().collect())
                        .unwrap_or_default()
                }
                None => String::new(),
            },
            Some(ParamOp::UpperFirst) => upper_first(&value),
            Some(ParamOp::UpperAll) => value.to_uppercase(),
            Some(ParamOp::LowerFirst) => lower_first(&value),
            Some(ParamOp::LowerAll) => value.to_lowercase(),
            Some(ParamOp::Indirect) => {
                if !defined {
                    self.fail_indirect(&parameter.name, "invalid indirect expansion");
                    String::new()
                } else if !is_indirect_name(&value) {
                    self.fail_indirect(&value, "invalid variable name");
                    String::new()
                } else if self.nounset
                    && !is_special_name(&value)
                    && !self.arrays.contains_key(&value)
                    && !self.env.iter().any(|(key, _)| key == &value)
                {
                    // Bash blames the indirect parameter itself, and treats the
                    // failure as a nounset violation rather than an invalid
                    // indirection.
                    eprintln!("sharky: !{}: unbound variable", parameter.name);
                    self.nounset_failed = true;
                    String::new()
                } else {
                    self.var(&value)
                }
            }
            // A subscript that is neither `@` nor `*` turns `${!name[sub]}`
            // into an indirection through the selected element.
            Some(ParamOp::Keys) => {
                if !defined {
                    String::new()
                } else if !is_indirect_name(&value) {
                    self.fail_indirect(&value, "invalid variable name");
                    String::new()
                } else {
                    self.var(&value)
                }
            }
            Some(ParamOp::Names) => {
                let mut names: Vec<String> = self
                    .env
                    .iter()
                    .filter(|(key, _)| key.starts_with(&parameter.name))
                    .map(|(key, _)| key.clone())
                    .collect();
                names.extend(
                    self.arrays
                        .keys()
                        .filter(|key| key.starts_with(&parameter.name))
                        .cloned(),
                );
                names.sort();
                names.dedup();
                names.join(" ")
            }
            Some(op @ (ParamOp::Substitute | ParamOp::SubstituteAll)) => {
                let (pattern, replacement) = argument
                    .split_once('/')
                    .unwrap_or((argument.as_str(), ""));
                if pattern.is_empty() {
                    value
                } else {
                    substitute_pattern(
                        &value,
                        pattern,
                        replacement,
                        op == ParamOp::SubstituteAll,
                        self.extglob,
                        self.nocasematch,
                    )
                }
            }
            Some(ParamOp::Transform) => {
                self.transform_parameter(&target_name, &value, defined, &argument)
            }
        }
    }

    fn transform_parameter(&self, name: &str, value: &str, defined: bool, spec: &str) -> String {
        match spec {
            "Q" => {
                if !defined {
                    String::new()
                } else {
                    bash_single_quote(value)
                }
            }
            "U" => value.to_uppercase(),
            "u" => upper_first(value),
            "L" => value.to_lowercase(),
            "E" => unescape_ansi_c(value),
            "P" => value.to_string(),
            "a" => self.var_attribute_flags(name),
            "A" => {
                let attr = self.var_attribute_flags(name);
                if let Some(target_opt) = self.namerefs.get(name) {
                    return match target_opt {
                        Some(t) => format!("declare -{attr} {name}={}", bash_single_quote(t)),
                        None => format!("declare -{attr} {name}"),
                    };
                }
                if !defined {
                    if attr.is_empty() {
                        String::new()
                    } else {
                        format!("declare -{attr} {name}")
                    }
                } else if attr.is_empty() {
                    format!("{name}={}", bash_single_quote(value))
                } else {
                    format!("declare -{attr} {name}={}", bash_single_quote(value))
                }
            }
            _ => String::new(),
        }
    }

    fn parameter_elements(&mut self, parameter: &Parameter) -> Option<Vec<String>> {
        if parameter.index.is_none()
            && parameter.operator.is_none()
            && matches!(parameter.name.as_str(), "@" | "*")
        {
            return Some(self.positional.clone());
        }

        let (target_name, nameref_sub) = match self.resolve_nameref(&parameter.name) {
            Ok(resolved) => resolved,
            Err(err) => {
                eprintln!("{err}");
                self.expansion_error = true;
                return Some(Vec::new());
            }
        };
        let index = parameter.index.as_deref().or(nameref_sub.as_deref());

        match parameter.operator {
            Some(ParamOp::Keys) => {
                // Only the `@` and `*` subscripts list the keys; a numeric or
                // associative subscript indirects through the element instead.
                if !matches!(index, Some("@") | Some("*")) {
                    return None;
                }
                if let Some(assoc) = self.assoc.get(&target_name) {
                    return Some(assoc.keys().cloned().collect());
                }
                if let Some(array) = self.arrays.get(&target_name) {
                    return Some(
                        array
                            .iter()
                            .enumerate()
                            .filter(|(_, value)| value.is_some())
                            .map(|(index, _)| index.to_string())
                            .collect(),
                    );
                }
                return Some(self.scalar_key(&target_name).into_iter().collect());
            }
            Some(ParamOp::Names) => {
                let mut names: Vec<String> = self
                    .env
                    .iter()
                    .filter(|(key, _)| key.starts_with(&parameter.name))
                    .map(|(key, _)| key.clone())
                    .collect();
                names.extend(
                    self.arrays
                        .keys()
                        .filter(|key| key.starts_with(&parameter.name))
                        .cloned(),
                );
                names.extend(
                    self.assoc
                        .keys()
                        .filter(|key| key.starts_with(&parameter.name))
                        .cloned(),
                );
                names.sort();
                names.dedup();
                return Some(names);
            }
            Some(op @ (ParamOp::UpperFirst | ParamOp::UpperAll | ParamOp::LowerFirst | ParamOp::LowerAll)) => {
                let index = index?;
                if index != "@" && index != "*" {
                    return None;
                }
                let values = if let Some(assoc) = self.assoc.get(&target_name) {
                    assoc.values().cloned().collect::<Vec<_>>()
                } else {
                    self.arrays
                        .get(&target_name)
                        .map(|values| values.iter().flatten().cloned().collect())
                        .unwrap_or_default()
                };
                return Some(
                    values
                        .into_iter()
                        .map(|value| match op {
                            ParamOp::UpperFirst => upper_first(&value),
                            ParamOp::UpperAll => value.to_uppercase(),
                            ParamOp::LowerFirst => lower_first(&value),
                            ParamOp::LowerAll => value.to_lowercase(),
                            _ => value,
                        })
                        .collect(),
                );
            }
            Some(ParamOp::Transform) => {
                let index = index?;
                if index != "@" && index != "*" {
                    return None;
                }
                let spec = parameter.argument.as_deref().unwrap_or("");
                let values = if let Some(assoc) = self.assoc.get(&target_name) {
                    assoc.values().cloned().collect::<Vec<_>>()
                } else {
                    self.arrays
                        .get(&target_name)
                        .map(|values| values.iter().flatten().cloned().collect())
                        .unwrap_or_default()
                };
                return Some(
                    values
                        .into_iter()
                        .map(|value| self.transform_parameter(&target_name, &value, true, spec))
                        .collect(),
                );
            }
            None | Some(ParamOp::Substring) => {}
            _ => return None,
        }

        if (parameter.name == "@" || parameter.name == "*")
            && parameter.operator == Some(ParamOp::Substring)
        {
            let argument = parameter.argument.as_deref().unwrap_or("");
            let Some((offset, length)) = self.slice_parts(argument) else {
                return Some(Vec::new());
            };
            let mut values = self.positional.clone();
            let offset = if offset > 0 { offset - 1 } else { offset };
            let Some((start, end)) = bound_slice(values.len(), offset, length) else {
                return Some(Vec::new());
            };
            values = values[start..end].to_vec();
            return Some(values);
        }

        let index = index?;
        if index != "@" && index != "*" {
            return None;
        }

        if parameter.operator == Some(ParamOp::Substring) {
            let argument = parameter.argument.as_deref().unwrap_or("");
            let Some((offset, length)) = self.slice_parts(argument) else {
                return Some(Vec::new());
            };
            if let Some(array) = self.arrays.get(&target_name) {
                let entries: Vec<(i64, String)> = array
                    .iter()
                    .enumerate()
                    .filter_map(|(index, value)| {
                        value.as_ref().map(|value| (index as i64, value.clone()))
                    })
                    .collect();
                let max_index = array
                    .iter()
                    .rposition(|value| value.is_some())
                    .unwrap_or(0) as i64;
                let start = if offset < 0 {
                    let start = max_index + 1 + offset;
                    if start < 0 {
                        return Some(Vec::new());
                    }
                    start
                } else {
                    offset
                };
                let selected: Vec<String> = entries
                    .into_iter()
                    .filter(|(index, _)| *index >= start)
                    .map(|(_, value)| value)
                    .collect();
                return Some(apply_count(selected, length));
            }
            let mut values = self
                .assoc
                .get(&target_name)
                .map(|assoc| assoc.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let Some((start, end)) = bound_slice(values.len(), offset, length) else {
                return Some(Vec::new());
            };
            values = values[start..end].to_vec();
            return Some(values);
        }

        Some(
            self.arrays
                .get(&target_name)
                .map(|values| values.iter().flatten().cloned().collect())
                .or_else(|| {
                    self.assoc
                        .get(&target_name)
                        .map(|assoc| assoc.values().cloned().collect())
                })
                .unwrap_or_default(),
        )
    }

    fn arithmetic_index(&mut self, text: &str) -> Option<i64> {
        let text = text.trim();
        if text.is_empty() {
            return Some(0);
        }
        match arith::evaluate(text, self) {
            Ok(value) => Some(value),
            Err(arith::ArithError::Readonly) => {
                self.expansion_error = true;
                None
            }
            Err(arith::ArithError::Message(message)) => {
                eprintln!("sharky: arithmetic: {message}");
                self.expansion_error = true;
                None
            }
        }
    }

    fn slice_parts(&mut self, argument: &str) -> Option<(i64, Option<i64>)> {
        let (offset, length) = match argument.split_once(':') {
            Some((offset, length)) => (offset, Some(length)),
            None => (argument, None),
        };
        let offset = self.arithmetic_index(offset)?;
        let length = match length {
            Some(text) => Some(self.arithmetic_index(text)?),
            None => None,
        };
        Some((offset, length))
    }

    fn process_substitution(&mut self, source: &str, input: bool) -> String {
        if std::env::var_os("SHARKY_PROCESS_SUBST_FIFO").is_none()
            && Path::new("/dev/fd").exists()
        {
            return self.pipe_substitution(source, input);
        }
        self.fifo_substitution(source, input)
    }

    fn pipe_substitution(&mut self, source: &str, input: bool) -> String {
        let Ok((read, write)) = pipe2(OFlag::empty()) else {
            return String::new();
        };

        let mask = crate::signal::block_all();
        // SAFETY: the child only dup2s pipe ends, runs shell code and exits.
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                if self.isolate_groups {
                    let group = self.job_pgid.unwrap_or_else(getpid);
                    let _ = setpgid(Pid::from_raw(0), group);
                    self.job_pgid = Some(group);
                } else {
                    self.job_pgid = None;
                }
                crate::agent::child_guard();
                self.reset_child_traps();
                crate::signal::restore_mask(mask);
                self.interactive = false;
                let keep = if input {
                    write.as_raw_fd()
                } else {
                    read.as_raw_fd()
                };
                let target = if input { 1 } else { 0 };
                // SAFETY: dup2 and close operate on the pipe fds owned here.
                unsafe {
                    let _ = nix::libc::dup2(keep, target);
                    let _ = nix::libc::close(read.as_raw_fd());
                    let _ = nix::libc::close(write.as_raw_fd());
                    // The child only needs stdio; inherited pipeline ends would
                    // otherwise keep the parent's pipes open and deadlock.
                    for fd in 3..1024 {
                        let _ = nix::libc::close(fd);
                    }
                }
                let status = match self.eval_substitution(source) {
                    Ok(code) => code,
                    Err(err) => {
                        eprintln!("sharky: {}", err.message);
                        2
                    }
                };
                unsafe { _exit(status) }
            }
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                if self.isolate_groups {
                    let group = self.job_pgid.unwrap_or(child);
                    let _ = setpgid(child, group);
                    if self.job_pgid.is_none() {
                        crate::agent::note_group(child.as_raw());
                    }
                }
                self.substitution_pids.push(child);
                let fd = if input {
                    drop(write);
                    read.into_raw_fd()
                } else {
                    drop(read);
                    write.into_raw_fd()
                };
                self.substitution_fds.push(fd);
                self.substitution_paths.push(String::new());
                self.substitution_ready.push(-1);
                format!("/dev/fd/{fd}")
            }
            Err(_) => {
                crate::signal::restore_mask(mask);
                String::new()
            }
        }
    }

    fn fifo_substitution(&mut self, source: &str, input: bool) -> String {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;

        let mut attempt = 0;
        let dirs = crate::agent::capture::temp_dirs();
        let path = loop {
            if attempt >= dirs.len() * 8 {
                return String::new();
            }
            let Some(suffix) = random_hex(16) else {
                return String::new();
            };
            let dir = &dirs[attempt % dirs.len()];
            let path = dir.join(format!(
                "sharky-psub-{}-{suffix}",
                std::process::id()
            ));
            attempt += 1;
            match mkfifo(&path, Mode::from_bits_truncate(0o600)) {
                Ok(()) => break path,
                Err(nix::errno::Errno::EEXIST) => continue,
                // A missing or unwritable directory is not fatal: try the
                // next candidate from TMPDIR, /tmp, /var/tmp or the cwd.
                Err(nix::errno::Errno::ENOENT)
                | Err(nix::errno::Errno::EACCES)
                | Err(nix::errno::Errno::ENOTDIR) => continue,
                Err(_) => return String::new(),
            }
        };
        let path_text = path.to_string_lossy().into_owned();
        let Ok((ready_read, ready_write)) = pipe2(OFlag::O_CLOEXEC) else {
            let _ = std::fs::remove_file(&path);
            return String::new();
        };

        let mask = crate::signal::block_all();
        // SAFETY: the child opens the FIFO, runs shell code and exits.
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                if self.isolate_groups {
                    let group = self.job_pgid.unwrap_or_else(getpid);
                    let _ = setpgid(Pid::from_raw(0), group);
                    self.job_pgid = Some(group);
                } else {
                    self.job_pgid = None;
                }
                crate::agent::child_guard();
                self.reset_child_traps();
                crate::signal::restore_mask(mask);
                self.interactive = false;
                drop(ready_read);
                let fd = match open_fifo_end(&path, input) {
                    Some(fd) => fd,
                    None => {
                        signal_fifo_ready(ready_write.as_raw_fd(), 0);
                        unsafe { _exit(1) }
                    }
                };
                signal_fifo_ready(ready_write.as_raw_fd(), 1);
                let target = if input { 1 } else { 0 };
                unsafe {
                    let _ = nix::libc::dup2(fd.as_raw_fd(), target);
                    for fd in 3..1024 {
                        let _ = nix::libc::close(fd);
                    }
                }
                // Both ends are open now, so the name is no longer needed.
                // Unlinking here avoids any global temp-directory sweep.
                let _ = std::fs::remove_file(&path);
                let status = match self.eval_substitution(source) {
                    Ok(code) => code,
                    Err(err) => {
                        eprintln!("sharky: {}", err.message);
                        2
                    }
                };
                let _ = std::fs::remove_file(&path);
                unsafe { _exit(status) }
            }
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                drop(ready_write);
                if self.isolate_groups {
                    let group = self.job_pgid.unwrap_or(child);
                    let _ = setpgid(child, group);
                    if self.job_pgid.is_none() {
                        crate::agent::note_group(child.as_raw());
                    }
                }
                self.substitution_pids.push(child);
                self.substitution_fds.push(-1);
                self.substitution_paths.push(path_text.clone());
                self.substitution_ready.push(ready_read.into_raw_fd());
                path_text
            }
            Err(_) => {
                crate::signal::restore_mask(mask);
                drop(ready_read);
                drop(ready_write);
                let _ = std::fs::remove_file(&path);
                String::new()
            }
        }
    }

    fn reap_substitutions(&mut self) {
        self.substitution_pids
            .retain(|pid| matches!(waitpid(*pid, Some(WaitPidFlag::WNOHANG)), Ok(WaitStatus::StillAlive)));
    }

    pub fn reap_substitutions_idle(&mut self) {
        self.reap_substitutions();
    }

    pub fn has_substitutions(&self) -> bool {
        !self.substitution_pids.is_empty()
    }

    pub fn close_substitutions_from(&mut self, start: usize) {
        while self.substitution_fds.len() > start {
            let ready = self.substitution_ready.pop().unwrap_or(-1);
            if let Some(fd) = self.substitution_fds.pop()
                && fd >= 0
            {
                let _ = close(fd);
            }
            if let Some(path) = self.substitution_paths.pop()
                && !path.is_empty()
            {
                let wake = wake_fifo(&path);
                if ready >= 0 {
                    // The readiness byte is only a barrier: once the child has
                    // opened the FIFO it is safe to drop `wake` and unlink the
                    // path. Bound the wait so a stopped child cannot hang the
                    // shell indefinitely; drain the byte if it did arrive.
                    if wait_fifo_ready(ready) {
                        let mut byte = [0u8; 1];
                        // SAFETY: `ready` is the read end of this substitution's
                        // readiness pipe, and poll reported a byte available.
                        unsafe {
                            let _ = nix::libc::read(
                                ready,
                                byte.as_mut_ptr() as *mut _,
                                byte.len(),
                            );
                        }
                    }
                    // SAFETY: closing a descriptor we own exactly once.
                    unsafe {
                        let _ = nix::libc::close(ready);
                    }
                }
                drop(wake);
                let _ = std::fs::remove_file(path);
            } else if ready >= 0 {
                let _ = close(ready);
            }
        }
    }

    fn command_substitution(&mut self, source: &str) -> String {
        // Bash reads `$(<file)` without forking when the body is nothing but
        // one input redirection. Everything else is still executed as shell
        // code in a subshell.
        if let Some(target) = redirect_only_input_target(source) {
            return self.file_substitution(&target);
        }

        let Ok((read, write)) = pipe2(OFlag::O_CLOEXEC) else {
            return String::new();
        };

        let mask = crate::signal::block_all();
        // SAFETY: the child runs shell code and exits without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                drop(write);
                let supervised_foreground = self.isolate_groups && self.job_pgid.is_none();
                if self.isolate_groups {
                    let group = self.job_pgid.unwrap_or(child);
                    let _ = setpgid(child, group);
                    if supervised_foreground {
                        crate::agent::note_foreground(child.as_raw());
                    }
                }
                let mut captured = Vec::new();
                let status = if crate::agent::active() {
                    let read_fd = read.as_raw_fd();
                    // SAFETY: `read_fd` is owned by `read` until dropped after
                    // the read loop; setting `O_NONBLOCK` after fork leaves the
                    // child's write end blocking.
                    unsafe {
                        let flags = nix::libc::fcntl(read_fd, nix::libc::F_GETFL);
                        if flags >= 0 {
                            nix::libc::fcntl(
                                read_fd,
                                nix::libc::F_SETFL,
                                flags | nix::libc::O_NONBLOCK,
                            );
                        }
                    }
                    let mut chunk = [0u8; 8192];
                    loop {
                        if crate::signal::fatal_signal().is_some() {
                            break;
                        }
                        crate::agent::service();
                        if crate::signal::fatal_signal().is_some() {
                            break;
                        }
                        // SAFETY: `chunk` is valid for writes of `chunk.len()`.
                        let count = unsafe {
                            nix::libc::read(
                                read_fd,
                                chunk.as_mut_ptr() as *mut nix::libc::c_void,
                                chunk.len(),
                            )
                        };
                        if count == 0 {
                            break;
                        }
                        if count > 0 {
                            captured.extend_from_slice(&chunk[..count as usize]);
                            continue;
                        }
                        match std::io::Error::last_os_error().kind() {
                            std::io::ErrorKind::Interrupted => continue,
                            std::io::ErrorKind::WouldBlock => {
                                if crate::signal::fatal_signal().is_some() {
                                    break;
                                }
                                crate::agent::wait_poll_fd(read_fd);
                            }
                            _ => break,
                        }
                    }
                    drop(read);
                    let status = match self.wait_foreground_member(child) {
                        Some(WaitStatus::Exited(_, code)) => code,
                        Some(WaitStatus::Signaled(_, signal, _)) => 128 + signal as i32,
                        _ => crate::signal::fatal_signal().map_or(1, |signal| 128 + signal),
                    };
                    if supervised_foreground {
                        crate::agent::clear_foreground();
                    }
                    status
                } else {
                    use std::io::Read;
                    use std::os::fd::FromRawFd;

                    // SAFETY: `read` transfers sole ownership of the descriptor.
                    let mut file = unsafe { std::fs::File::from_raw_fd(read.into_raw_fd()) };
                    let _ = file.read_to_end(&mut captured);
                    let status = exec::wait(child);
                    if supervised_foreground {
                        crate::agent::clear_foreground();
                    }
                    status
                };
                self.substitution_status = Some(status);
                // Reading into a String failed on the first byte that was not
                // valid UTF-8 and left the whole capture empty. The capture is
                // bytes now; decoding is lossy only because a word is a String
                // here, and bash's rule for NUL bytes is applied first.
                let mut output =
                    String::from_utf8_lossy(&discard_null_bytes(captured)).into_owned();
                while output.ends_with('\n') {
                    output.pop();
                }
                output
            }
            Ok(ForkResult::Child) => {
                if self.isolate_groups {
                    let group = self.job_pgid.unwrap_or_else(getpid);
                    let _ = setpgid(Pid::from_raw(0), group);
                    self.job_pgid = Some(group);
                } else {
                    self.job_pgid = None;
                }
                crate::agent::child_guard();
                self.reset_child_traps();
                crate::signal::restore_mask(mask);
                self.interactive = false;
                // Bash disables errexit inside `$(...)` unless `inherit_errexit`
                // is enabled, so a failing command there only shows up through
                // the substitution's own final status.
                self.errexit = false;
                let read_fd = read.into_raw_fd();
                let write_fd = write.into_raw_fd();
                let _ = move_fd(write_fd, 1);
                // SAFETY: `read_fd` and `write_fd` came from `pipe2` above and
                // we skip fd 1 because `move_fd` just bound it to the pipe.
                unsafe {
                    if read_fd != 1 {
                        nix::libc::close(read_fd);
                    }
                    if write_fd != 1 {
                        nix::libc::close(write_fd);
                    }
                }
                let code = match parser::parse(source) {
                    Ok(program) => {
                        self.shlvl_tail = if self.in_pipeline_member {
                            None
                        } else {
                            final_simple(&program)
                        };
                        flow_status(self.run_program(&program))
                    }
                    Err(err) => {
                        eprintln!("sharky: {}", err.message);
                        2
                    }
                };
                unsafe { _exit(code) }
            }
            Err(_) => {
                crate::signal::restore_mask(mask);
                String::new()
            }
        }
    }

    // The no-fork counterpart of `command_substitution` for `$(<file)`. The
    // target is expanded by the same machinery a redirect would use in the
    // subshell, and the bytes go through the same NUL/newline handling as any
    // other substitution result.
    fn file_substitution(&mut self, target: &Word) -> String {
        use std::io::Read;

        let redirect = Redirect {
            fd: 0,
            op: RedirectOp::In,
            target: Some(target.clone()),
        };
        let actions = match self.redirect_actions(std::slice::from_ref(&redirect)) {
            Ok(actions) => actions,
            Err(message) => {
                self.clear_substitution_expansion_failure();
                if !message.is_empty() {
                    eprintln!("sharky: {message}");
                }
                self.substitution_status = Some(1);
                return String::new();
            }
        };
        let expansion_failed = self.nounset_failed || self.expansion_error;
        self.clear_substitution_expansion_failure();
        if expansion_failed {
            self.substitution_status = Some(1);
            return String::new();
        }
        let Action::Read(_, path) = &actions[0] else {
            self.substitution_status = Some(1);
            return String::new();
        };

        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("sharky: {}: {}", path.display(), err);
                self.substitution_status = Some(1);
                return String::new();
            }
        };
        let mut bytes = Vec::new();
        let _ = file.read_to_end(&mut bytes);
        self.substitution_status = Some(0);
        let mut output = String::from_utf8_lossy(&discard_null_bytes(bytes)).into_owned();
        while output.ends_with('\n') {
            output.pop();
        }
        output
    }

    // Expansion happens in this shell rather than in the forked subshell, so
    // pending failure flags must not escape into the following command.
    fn clear_substitution_expansion_failure(&mut self) {
        self.nounset_failed = false;
        self.expansion_error = false;
        self.null_parameter_error = false;
    }

    fn glob_fields(&mut self, fields: Vec<(String, bool)>) -> Vec<String> {
        let mut out = Vec::with_capacity(fields.len());

        for (text, eligible) in fields {
            if eligible && has_glob_opts(&text, self.extglob) {
                let matches = glob_with_opts(&text, self.globstar, self.dotglob, self.extglob);
                if !matches.is_empty() {
                    out.extend(matches);
                    continue;
                }
                if self.failglob {
                    eprintln!("sharky: no match: {}", strip_quoted_glob_markers(&text));
                    self.expansion_error = true;
                    return Vec::new();
                }
                if self.nullglob {
                    continue;
                }
            }
            if text.contains(QUOTED_GLOB_MARKER) {
                out.push(strip_quoted_glob_markers(&text));
            } else {
                out.push(text);
            }
        }

        out
    }
}

fn open_fifo_end(path: &Path, write: bool) -> Option<std::os::fd::OwnedFd> {
    use nix::fcntl::{OFlag, open};
    use nix::sys::stat::Mode;

    let flags = if write {
        OFlag::O_WRONLY
    } else {
        OFlag::O_RDONLY
    };
    // A symlink planted at the path between mkfifo and this open must be
    // refused, not followed to an arbitrary file.
    open(path, flags | OFlag::O_NOFOLLOW, Mode::empty()).ok()
}

fn wake_fifo(path: &str) -> Option<std::os::fd::OwnedFd> {
    use nix::fcntl::{OFlag, open};
    use nix::sys::stat::Mode;
    use std::os::unix::fs::FileTypeExt;

    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return None;
    };
    if !meta.file_type().is_fifo() {
        return None;
    }
    // O_NOFOLLOW closes the metadata-to-open race: if the path turns into a
    // symlink after the check above, refuse it instead of opening the target.
    open(
        Path::new(path),
        OFlag::O_RDWR | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .ok()
}

fn signal_fifo_ready(fd: RawFd, status: u8) {
    // A process-substitution child can be exec'd before the parent consumes
    // this handshake pipe. Ignore SIGPIPE for the optional notification.
    unsafe {
        let previous = nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_IGN);
        let _ = nix::libc::write(fd, &status as *const u8 as *const _, 1);
        let _ = nix::libc::signal(nix::libc::SIGPIPE, previous);
    }
}

/// Wait for the process-substitution child's readiness byte without ever
/// blocking indefinitely. A child stopped before it signals ready (for example
/// by Ctrl-Z during expansion) used to freeze the interactive shell forever on
/// a bare `read`. `poll` with a finite budget keeps the wait responsive, retries
/// on `EINTR`, and never busy-waits. A healthy child signals in microseconds,
/// so the timeout only ever fires for a stopped or wedged child.
fn wait_fifo_ready(fd: RawFd) -> bool {
    use nix::errno::Errno;
    use nix::libc::{POLLIN, poll, pollfd};

    let mut descriptor = pollfd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    // 500 ms total in 50 ms slices: far longer than a local fork + fifo open +
    // pipe write needs, but short enough that an interactive shell stays
    // responsive if the child never arrives.
    let mut remaining = 10;
    while remaining > 0 {
        let result = unsafe { poll(&mut descriptor, 1, 50) };
        if result > 0 {
            return descriptor.revents & POLLIN != 0;
        }
        if result < 0 {
            if Errno::last() == Errno::EINTR {
                continue;
            }
            return false;
        }
        remaining -= 1;
    }
    false
}

/// A name bash accepts as the target of an indirect expansion: an identifier
/// or a positional parameter number.
fn is_indirect_name(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
        }
        Some(first) if first.is_ascii_digit() => chars.all(|char| char.is_ascii_digit()),
        _ => false,
    }
}

fn joins_elements(parameter: &Parameter) -> bool {
    parameter.index.as_deref() == Some("*")
        || (parameter.name == "*" && parameter.index.is_none() && parameter.operator.is_none())
        || (parameter.name == "*" && parameter.operator == Some(ParamOp::Substring))
        || (parameter.operator == Some(ParamOp::Names)
            && parameter.argument.as_deref() == Some("*"))
}

fn apply_count(values: Vec<String>, length: Option<i64>) -> Vec<String> {
    match length {
        Some(length) if length >= 0 => values.into_iter().take(length as usize).collect(),
        Some(length) => {
            let keep = values.len().saturating_sub((-length) as usize);
            values.into_iter().take(keep).collect()
        }
        None => values,
    }
}

fn unquote_subscript(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(char) = chars.next() {
        match char {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '\'' => {
                for next in chars.by_ref() {
                    if next == '\'' {
                        break;
                    }
                    out.push(next);
                }
            }
            '"' => {
                while let Some(next) = chars.next() {
                    if next == '"' {
                        break;
                    }
                    if next == '\\'
                        && let Some(escaped) = chars.next()
                    {
                        out.push(escaped);
                        continue;
                    }
                    out.push(next);
                }
            }
            char => out.push(char),
        }
    }
    out
}

fn bound_slice(len: usize, offset: i64, length: Option<i64>) -> Option<(usize, usize)> {
    let len = len as i64;
    let start = if offset < 0 {
        match len.checked_add(offset) {
            Some(start) if start >= 0 => start,
            _ => return None,
        }
    } else {
        offset.min(len)
    };
    let end = match length {
        Some(length) if length < 0 => len.checked_add(length).unwrap_or(0).max(start),
        Some(length) => start.saturating_add(length).min(len),
        None => len,
    };
    Some((start as usize, end.max(start) as usize))
}

/// Split a `[subscript]=value` element of an array literal into its subscript
/// word and its value word. A quoted or escaped leading bracket is not a
/// subscript, matching the value bash stores for it.
fn split_indexed_element(word: &Word) -> Option<(Word, Word)> {
    let mut index_parts = Vec::new();
    for (position, part) in word.parts.iter().enumerate() {
        let Part::Literal(text) = part else {
            index_parts.push(part.clone());
            continue;
        };
        let text = match position {
            0 => text.strip_prefix('[')?,
            _ => text.as_str(),
        };
        let Some(close) = text.find("]=") else {
            if !text.is_empty() {
                index_parts.push(Part::Literal(text.to_string()));
            }
            continue;
        };
        let subscript = &text[..close];
        if subscript.is_empty() && index_parts.is_empty() {
            return None;
        }
        if !subscript.is_empty() {
            index_parts.push(Part::Literal(subscript.to_string()));
        }
        let mut value_parts = Vec::new();
        let rest = &text[close + 2..];
        if !rest.is_empty() {
            value_parts.push(Part::Literal(rest.to_string()));
        }
        value_parts.extend(word.parts[position + 1..].iter().cloned());
        return Some((Word { parts: index_parts }, Word { parts: value_parts }));
    }
    None
}

/// Pairs for an associative array literal: an explicit subscript keeps its
/// expanded text, while a bare value falls back to the `[key]=value` form.
fn assoc_entries(entries: &[(Option<String>, String)]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (index, value) in entries {
        match index {
            Some(index) => pairs.push((index.clone(), value.clone())),
            None => {
                if let Some(pair) = assoc_literal(value) {
                    pairs.push(pair);
                }
            }
        }
    }
    pairs
}

fn assoc_literal(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix('[')?;
    let close = rest.find("]=")?;
    if close == 0 {
        return None;
    }
    Some((rest[..close].to_string(), rest[close + 2..].to_string()))
}

fn split_array_argument(text: &str) -> Option<(String, String)> {
    let open = text.find('[')?;
    if !text.ends_with(']') {
        return None;
    }
    let name = &text[..open];
    let index = &text[open + 1..text.len() - 1];
    if name.is_empty() || index.is_empty() {
        return None;
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
    {
        return None;
    }
    Some((name.to_string(), index.to_string()))
}

fn upper_first(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::new();
    if first.is_alphabetic() {
        out.extend(first.to_uppercase());
    } else {
        out.push(first);
    }
    out.push_str(chars.as_str());
    out
}

fn lower_first(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::new();
    if first.is_alphabetic() {
        out.extend(first.to_lowercase());
    } else {
        out.push(first);
    }
    out.push_str(chars.as_str());
    out
}

type ExtGlob<'a> = (char, Vec<Vec<char>>, &'a [char]);

fn split_extglob(pattern: &[char]) -> Option<ExtGlob<'_>> {
    let operator = *pattern.first()?;
    if !matches!(operator, '?' | '*' | '+' | '@' | '!') || pattern.get(1) != Some(&'(') {
        return None;
    }

    let mut depth = 0usize;
    let mut end = None;
    let mut quote = None;
    let mut index = 1;
    while index < pattern.len() {
        let char = &pattern[index];
        if let Some(delimiter) = quote {
            if *char == delimiter {
                quote = None;
            }
            index += 1;
            continue;
        }
        match char {
            '\u{1}' => index += 2,
            '\\' => index += 2,
            '\'' | '"' => {
                quote = Some(*char);
                index += 1;
            }
            '(' => {
                depth += 1;
                index += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end = Some(index);
                    break;
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    let end = end?;

    let mut alternatives = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut index = 2;
    while index < end {
        let char = pattern[index];
        if let Some(delimiter) = quote {
            current.push(char);
            index += 1;
            if char == delimiter {
                quote = None;
            }
            continue;
        }
        match char {
            '\u{1}' | '\\' => {
                current.push(char);
                if let Some(next) = pattern.get(index + 1) {
                    current.push(*next);
                }
                index += 2;
            }
            '\'' | '"' => {
                quote = Some(char);
                current.push(char);
                index += 1;
            }
            '(' => {
                depth += 1;
                current.push(char);
                index += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(char);
                index += 1;
            }
            '|' if depth == 0 => {
                alternatives.push(std::mem::take(&mut current));
                index += 1;
            }
            _ => {
                current.push(char);
                index += 1;
            }
        }
    }
    alternatives.push(current);

    Some((operator, alternatives, &pattern[end + 1..]))
}

fn ext_matches(operator: char, alternatives: &[Vec<char>], text: &[char]) -> bool {
    let exact = |text: &[char]| {
        alternatives.iter().any(|alt| {
            let alt = normalize_pattern(alt);
            glob_chars(&alt, text)
        })
    };
    match operator {
        '?' => text.is_empty() || exact(text),
        '@' => exact(text),
        '*' => text.is_empty() || ext_repeat(alternatives, text),
        '+' => {
            ext_repeat(alternatives, text)
                && (!text.is_empty()
                    || alternatives.iter().any(|alt| {
                        let alt = normalize_pattern(alt);
                        glob_chars(&alt, &[])
                    }))
        }
        '!' => !exact(text),
        _ => false,
    }
}

fn quote_pattern(out: &mut String, text: &str) {
    for char in text.chars() {
        out.push('\u{1}');
        out.push(char);
    }
}

fn normalize_pattern(pattern: &[char]) -> Vec<char> {
    let mut out = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for &char in pattern {
        if escaped {
            out.push('\u{1}');
            out.push(char);
            escaped = false;
            continue;
        }
        match quote {
            Some(delimiter) => {
                if char == delimiter {
                    quote = None;
                } else {
                    out.push('\u{1}');
                    out.push(char);
                }
            }
            None => match char {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(char),
                _ => out.push(char),
            },
        }
    }
    out
}

fn ext_repeat(alternatives: &[Vec<char>], text: &[char]) -> bool {
    let mut reachable = vec![false; text.len() + 1];
    reachable[0] = true;
    for end in 1..=text.len() {
        for start in 0..end {
            if reachable[start]
                && alternatives
                    .iter()
                    .any(|alt| {
                        let alt = normalize_pattern(alt);
                        glob_chars(&alt, &text[start..end])
                    })
            {
                reachable[end] = true;
                break;
            }
        }
    }
    reachable[text.len()]
}

fn cpu_usage(who: i32) -> (f64, f64) {
    // SAFETY: getrusage writes into a zeroed struct of the correct type.
    let mut usage: nix::libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { nix::libc::getrusage(who, &mut usage) } != 0 {
        return (0.0, 0.0);
    }
    let user = usage.ru_utime.tv_sec as f64 + usage.ru_utime.tv_usec as f64 / 1_000_000.0;
    let sys = usage.ru_stime.tv_sec as f64 + usage.ru_stime.tv_usec as f64 / 1_000_000.0;
    (user, sys)
}

fn report_time(real: f64, user: f64, sys: f64, posix: bool) {
    if posix {
        eprintln!("real {real:.2}\nuser {user:.2}\nsys {sys:.2}");
        return;
    }

    eprintln!(
        "\nreal\t{}\nuser\t{}\nsys\t{}",
        format_seconds(real),
        format_seconds(user),
        format_seconds(sys)
    );
}

fn format_seconds(seconds: f64) -> String {
    format!("{}m{:.3}s", (seconds / 60.0).floor() as u64, seconds % 60.0)
}

/// Whether a completed command replaces `PIPESTATUS` with an array holding its
/// own status. Bash does that for every pipeline and every simple command,
/// including a subshell or a redirection wrapping one, but not for a group,
/// loop, conditional or function definition, which keep whatever array their
/// last inner command published.
fn publishes_pipe_status(command: &Command) -> bool {
    match command {
        Command::Simple(_) | Command::Test(_) | Command::Arithmetic(_) | Command::Subshell(_) => {
            true
        }
        Command::Not(inner) => publishes_pipe_status(inner),
        Command::Redirected { command, .. } => publishes_pipe_status(command),
        _ => false,
    }
}

fn describe_and_or(and_or: &AndOr) -> String {
    let mut text = describe_pipeline(&and_or.first);
    for (is_and, pipeline) in &and_or.rest {
        text.push_str(if *is_and { " && " } else { " || " });
        text.push_str(&describe_pipeline(pipeline));
    }
    text
}

fn describe_list(list: &List) -> String {
    list.entries
        .iter()
        .map(|entry| {
            let text = describe_and_or(&entry.and_or);
            if entry.background {
                format!("{text} &")
            } else {
                text
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn describe_pipeline(pipeline: &Pipeline) -> String {
    pipeline
        .commands
        .iter()
        .map(describe_command)
        .collect::<Vec<_>>()
        .join(" | ")
}

fn describe_command(command: &Command) -> String {
    match command {
        Command::Simple(simple) => simple
            .words
            .iter()
            .filter_map(literal)
            .collect::<Vec<_>>()
            .join(" "),
        Command::Not(inner) => format!("! {}", describe_command(inner)),
        Command::Redirected { command, .. } => describe_command(command),
        Command::Coproc { name, command } => {
            format!("coproc {name} {}", describe_command(command))
        }
        Command::Subshell(_) => "( ... )".to_string(),
        Command::Group(_) => "{ ...; }".to_string(),
        Command::If { .. } => "if ...".to_string(),
        Command::For { name, .. } => format!("for {name} ..."),
        Command::ForArith { .. } => "for ((...))".to_string(),
        Command::While { until, .. } => {
            if *until {
                "until ...".to_string()
            } else {
                "while ...".to_string()
            }
        }
        Command::Case { .. } => "case ...".to_string(),
        Command::Function { name, .. } => format!("{name}()"),
        Command::Test(_) => "[[ ... ]]".to_string(),
        Command::Arithmetic(_) => "(( ... ))".to_string(),
    }
}

// bash prints a function definition in the same layout `declare -f` uses: the
// name followed by an empty parameter list, then the body in braces with one
// statement per line. `command -V` reports it after the description.
fn format_function_definition(name: &str, body: &List) -> String {
    let mut text = format!("{name} () \n{{ \n");
    text.push_str(&format_statements(body, 4, false));
    text.push_str("}\n");
    text
}

// bash ends every statement of a block closed by `}` with a semicolon except
// the last one; the bodies of `then`, `else` and `do` keep the semicolon on
// every statement.
fn format_statements(list: &List, indent: usize, always_semicolon: bool) -> String {
    let pad = " ".repeat(indent);
    let mut text = String::new();

    for (index, entry) in list.entries.iter().enumerate() {
        let last = index + 1 == list.entries.len();
        text.push_str(&pad);
        text.push_str(&format_entry(entry, indent));
        if !entry.background && (always_semicolon || !last) {
            text.push(';');
        }
        text.push('\n');
    }

    text
}

// A condition list prints its statements one per line, the way bash keeps them
// in the parse tree, so the keyword that follows goes after the last one.
fn format_condition(list: &List, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let separator = format!(";\n{pad}");
    list.entries
        .iter()
        .map(|entry| format_entry(entry, indent))
        .collect::<Vec<_>>()
        .join(&separator)
}

fn format_entry(entry: &Entry, indent: usize) -> String {
    let mut text = format_and_or(&entry.and_or, indent);
    if entry.background {
        text.push_str(" &");
    }
    text
}

fn format_and_or(and_or: &AndOr, indent: usize) -> String {
    let mut text = format_pipeline(&and_or.first, indent);
    for (is_and, pipeline) in &and_or.rest {
        text.push_str(if *is_and { " && " } else { " || " });
        text.push_str(&format_pipeline(pipeline, indent));
    }
    text
}

fn format_pipeline(pipeline: &Pipeline, indent: usize) -> String {
    let mut text = String::new();
    if pipeline.timed.is_some() {
        text.push_str("time ");
    }
    if pipeline.negate {
        text.push_str("! ");
    }
    text.push_str(
        &pipeline
            .commands
            .iter()
            .map(|command| format_command(command, indent))
            .collect::<Vec<_>>()
            .join(" | "),
    );
    text
}

fn format_command(command: &Command, indent: usize) -> String {
    let pad = " ".repeat(indent);

    match command {
        Command::Simple(simple) => {
            let mut words: Vec<String> = Vec::new();
            for assignment in &simple.assignments {
                words.push(format_assignment(assignment));
            }
            for word in &simple.words {
                words.push(format_word(word));
            }
            let mut text = words.join(" ");
            for redirect in &simple.redirects {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(&format_redirect(redirect));
            }
            text
        }
        Command::Not(inner) => format!("! {}", format_command(inner, indent)),
        // bash keeps the statements of a subshell on the line that opens it and
        // continues at the same indentation.
        Command::Subshell(list) => {
            let separator = format!(";\n{pad}");
            let body = list
                .entries
                .iter()
                .map(|entry| format_entry(entry, indent))
                .collect::<Vec<_>>()
                .join(&separator);
            format!("( {body} )")
        }
        Command::Group(list) => {
            let mut text = String::from("{ \n");
            text.push_str(&format_statements(list, indent + 4, false));
            text.push_str(&pad);
            text.push('}');
            text
        }
        Command::If {
            branches,
            else_body,
        } => format_if(branches, else_body.as_ref(), indent),
        Command::For { name, words, body } => {
            let words = if words.is_empty() {
                "\"$@\"".to_string()
            } else {
                words
                    .iter()
                    .map(format_word)
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let mut text = format!("for {name} in {words};\n{pad}do\n");
            text.push_str(&format_statements(body, indent + 4, true));
            text.push_str(&pad);
            text.push_str("done");
            text
        }
        Command::ForArith {
            init,
            condition,
            step,
            body,
        } => {
            let mut text = format!("for (({init}; {condition}; {step}))\n{pad}do\n");
            text.push_str(&format_statements(body, indent + 4, true));
            text.push_str(&pad);
            text.push_str("done");
            text
        }
        Command::While {
            condition,
            body,
            until,
        } => {
            let mut text = format!("{} ", if *until { "until" } else { "while" });
            text.push_str(&format_condition(condition, indent));
            text.push_str("; do\n");
            text.push_str(&format_statements(body, indent + 4, true));
            text.push_str(&pad);
            text.push_str("done");
            text
        }
        Command::Case { word, arms } => {
            let mut text = format!("case {} in \n", format_word(word));
            let arm_pad = " ".repeat(indent + 4);
            for (patterns, body, term) in arms {
                let patterns = patterns
                    .iter()
                    .map(format_word)
                    .collect::<Vec<_>>()
                    .join(" | ");
                text.push_str(&arm_pad);
                text.push_str(&patterns);
                text.push_str(")\n");
                text.push_str(&format_statements(body, indent + 8, false));
                text.push_str(&arm_pad);
                text.push_str(match term {
                    CaseTerm::Break => ";;",
                    CaseTerm::Fallthrough => ";&",
                    CaseTerm::Continue => ";;&",
                });
                text.push('\n');
            }
            text.push_str(&pad);
            text.push_str("esac");
            text
        }
        Command::Function { name, body, .. } => {
            let mut text = format!("{name} () \n{pad}{{ \n");
            text.push_str(&format_statements(body, indent + 4, false));
            text.push_str(&pad);
            text.push('}');
            text
        }
        Command::Test(words) => format!(
            "[[ {} ]]",
            words
                .iter()
                .map(format_word)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Command::Arithmetic(source) => format!("(( {source} ))"),
        Command::Redirected {
            command,
            redirects,
        } => {
            let mut text = format_command(command, indent);
            for redirect in redirects {
                text.push(' ');
                text.push_str(&format_redirect(redirect));
            }
            text
        }
        Command::Coproc { name, command } => {
            format!("coproc {name} {}", format_command(command, indent))
        }
    }
}

// bash flattens `elif` into an `else` that holds a nested `if`, and keeps the
// semicolon on the nested one because it ends a statement of the else body.
fn format_if(branches: &[(List, List)], else_body: Option<&List>, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut text = String::new();

    let Some(((condition, body), rest)) = branches.split_first() else {
        return text;
    };
    text.push_str("if ");
    text.push_str(&format_condition(condition, indent));
    text.push_str("; then\n");
    text.push_str(&format_statements(body, indent + 4, true));

    if rest.is_empty() {
        if let Some(else_body) = else_body {
            text.push_str(&pad);
            text.push_str("else\n");
            text.push_str(&format_statements(else_body, indent + 4, true));
        }
    } else {
        text.push_str(&pad);
        text.push_str("else\n");
        text.push_str(&" ".repeat(indent + 4));
        text.push_str(&format_if(rest, else_body, indent + 4));
        text.push_str(";\n");
    }

    text.push_str(&pad);
    text.push_str("fi");
    text
}

fn format_assignment(assignment: &Assignment) -> String {
    let mut text = assignment.name.clone();
    if let Some(index) = &assignment.index {
        text.push('[');
        text.push_str(&format_word(index));
        text.push(']');
    }
    if assignment.append {
        text.push('+');
    }
    text.push('=');
    match &assignment.value {
        AssignmentValue::Word(word) => text.push_str(&format_word(word)),
        AssignmentValue::Array(words) => {
            text.push('(');
            text.push_str(&words.iter().map(format_word).collect::<Vec<_>>().join(" "));
            text.push(')');
        }
    }
    text
}

fn format_redirect(redirect: &Redirect) -> String {
    let fd = |number: i32, default: i32| {
        if number == default {
            String::new()
        } else {
            number.to_string()
        }
    };
    let target = redirect
        .target
        .as_ref()
        .map(format_word)
        .unwrap_or_default();

    match &redirect.op {
        RedirectOp::Out => format!("{}> {target}", fd(redirect.fd, 1)),
        RedirectOp::OutForce => format!("{}>| {target}", fd(redirect.fd, 1)),
        RedirectOp::Append => format!("{}>> {target}", fd(redirect.fd, 1)),
        RedirectOp::In => format!("{}< {target}", fd(redirect.fd, 0)),
        RedirectOp::Dup(other) => format!("{}>&{other}", fd(redirect.fd, 1)),
        RedirectOp::DupIn(other) => format!("{}<&{other}", fd(redirect.fd, 0)),
        RedirectOp::DupWord => format!("{}>&{target}", fd(redirect.fd, 1)),
        RedirectOp::DupInWord => format!("{}<&{target}", fd(redirect.fd, 0)),
        RedirectOp::Close => format!("{}>&-", fd(redirect.fd, 1)),
        RedirectOp::Here { body, .. } => format!("{}<< {body}", fd(redirect.fd, 0)),
        RedirectOp::HereString => format!("{}<<< {target}", fd(redirect.fd, 0)),
        RedirectOp::Both { append } => {
            let arrows = if *append { "&>>" } else { "&>" };
            format!("{arrows} {target}")
        }
    }
}

fn format_word(word: &Word) -> String {
    word.parts.iter().map(format_part).collect()
}

fn format_part(part: &Part) -> String {
    match part {
        Part::Literal(value) => value.clone(),
        Part::Single(value) => format!("'{value}'"),
        Part::Escaped(value) => format!("\\{value}"),
        Part::Double(inner) => {
            let inner: String = inner.iter().map(format_part).collect();
            format!("\"{inner}\"")
        }
        Part::Variable(name) => format!("${name}"),
        Part::Parameter(parameter) => format!("${{{}}}", format_parameter(parameter)),
        Part::CmdSub(source) => format!("$({source})"),
        Part::ProcessSub(source, input) => {
            format!("{}({source})", if *input { '<' } else { '>' })
        }
        Part::Arithmetic(source) => format!("$(({source}))"),
        Part::Tilde => "~".to_string(),
        Part::TildeUser(name) => format!("~{name}"),
    }
}

fn format_parameter(parameter: &Parameter) -> String {
    let index = parameter
        .index
        .as_ref()
        .map(|index| format!("[{index}]"))
        .unwrap_or_default();

    let symbol = match parameter.operator {
        None => return format!("{}{index}", parameter.name),
        Some(ParamOp::Length) => return format!("#{}{index}", parameter.name),
        Some(ParamOp::Indirect | ParamOp::Keys) => {
            return format!("!{}{index}", parameter.name);
        }
        Some(ParamOp::Names) => {
            let suffix = parameter.argument.clone().unwrap_or_default();
            return format!("!{}{suffix}", parameter.name);
        }
        Some(ParamOp::Default) => ":-",
        Some(ParamOp::DefaultUnset) => "-",
        Some(ParamOp::Assign) => ":=",
        Some(ParamOp::AssignUnset) => "=",
        Some(ParamOp::Alternate) => ":+",
        Some(ParamOp::AlternateUnset) => "+",
        Some(ParamOp::Error) => ":?",
        Some(ParamOp::ErrorUnset) => "?",
        Some(ParamOp::TrimPrefixShort) => "#",
        Some(ParamOp::TrimPrefixLong) => "##",
        Some(ParamOp::TrimSuffixShort) => "%",
        Some(ParamOp::TrimSuffixLong) => "%%",
        Some(ParamOp::Substitute) => "/",
        Some(ParamOp::SubstituteAll) => "//",
        Some(ParamOp::Substring) => ":",
        Some(ParamOp::UpperFirst) => "^",
        Some(ParamOp::UpperAll) => "^^",
        Some(ParamOp::LowerFirst) => ",",
        Some(ParamOp::LowerAll) => ",,",
        Some(ParamOp::Transform) => "@",
    };

    let argument = parameter.argument.clone().unwrap_or_default();
    format!("{}{index}{symbol}{argument}", parameter.name)
}

fn ensure_config_layout(dir: &Path) {
    // Do not populate an explicitly chosen config directory.
    if std::env::var_os("SHARKY_CONFIG").is_some() {
        return;
    }

    let config_file = dir.join("config.sharky");
    let theme_file = dir.join("theme.sharky");
    if config_file.exists() || theme_file.exists() {
        return;
    }

    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(config_file, include_str!("../examples/config.sharky"));
    let _ = std::fs::write(theme_file, include_str!("../examples/theme.sharky"));
}

fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SHARKY_CONFIG") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("sharky"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/sharky"))
}

fn reset_child_signals() {
    // SAFETY: called in a forked child before exec; restoring default
    // dispositions is required because ignored signals survive exec.
    unsafe {
        let _ = signal(Signal::SIGINT, SigHandler::SigDfl);
        let _ = signal(Signal::SIGQUIT, SigHandler::SigDfl);
        let _ = signal(Signal::SIGTSTP, SigHandler::SigDfl);
        let _ = signal(Signal::SIGTTOU, SigHandler::SigDfl);
        let _ = signal(Signal::SIGTTIN, SigHandler::SigDfl);
        let _ = signal(Signal::SIGPIPE, SigHandler::SigDfl);
        // The interactive shell's child watcher must not run in a child.
        let _ = signal(Signal::SIGCHLD, SigHandler::SigDfl);
        let _ = signal(Signal::SIGWINCH, SigHandler::SigDfl);
    }
}

fn flow_status(flow: Flow) -> i32 {
    match flow {
        Flow::Status(code) | Flow::Exit(code) | Flow::Discard(code) | Flow::Return(code) => code,
        Flow::Interrupt => 130,
        Flow::Break | Flow::Continue => 0,
    }
}

/// The status an arithmetic failure leaves in a construct that keeps running.
/// A refused write to a readonly variable already printed its message and only
/// reports 1; a syntax or division error reports 2.
fn arithmetic_failure(error: arith::ArithError) -> i32 {
    match error {
        arith::ArithError::Readonly => 1,
        arith::ArithError::Message(message) => {
            eprintln!("sharky: arithmetic: {message}");
            2
        }
    }
}

/// The status column bash prints in a job notice: `Done` for a clean exit and
/// `Exit N` otherwise.
fn exit_text(code: i32) -> (String, bool) {
    if code == 0 {
        ("Done".to_string(), false)
    } else {
        (format!("Exit {code}"), false)
    }
}

/// The description bash uses for a job that died from `signal`. The strings are
/// the ones bash 5.3 prints under `LC_ALL=C`.
fn signal_text(signal: i32) -> String {
    let text = match signal {
        nix::libc::SIGHUP => "Hangup",
        nix::libc::SIGINT => "Interrupt",
        nix::libc::SIGQUIT => "Quit",
        nix::libc::SIGILL => "Illegal instruction",
        nix::libc::SIGTRAP => "Trace/breakpoint trap",
        nix::libc::SIGABRT => "Aborted",
        nix::libc::SIGBUS => "Bus error",
        nix::libc::SIGFPE => "Floating point exception",
        nix::libc::SIGKILL => "Killed",
        nix::libc::SIGUSR1 => "User defined signal 1",
        nix::libc::SIGSEGV => "Segmentation fault",
        nix::libc::SIGUSR2 => "User defined signal 2",
        nix::libc::SIGPIPE => "Broken pipe",
        nix::libc::SIGALRM => "Alarm clock",
        nix::libc::SIGTERM => "Terminated",
        nix::libc::SIGSTKFLT => "Stack fault",
        nix::libc::SIGCHLD => "Child exited",
        nix::libc::SIGCONT => "Continued",
        nix::libc::SIGSTOP => "Stopped (signal)",
        nix::libc::SIGTSTP => "Stopped",
        nix::libc::SIGTTIN => "Stopped (tty input)",
        nix::libc::SIGTTOU => "Stopped (tty output)",
        nix::libc::SIGURG => "Urgent I/O condition",
        nix::libc::SIGXCPU => "CPU time limit exceeded",
        nix::libc::SIGXFSZ => "File size limit exceeded",
        nix::libc::SIGVTALRM => "Virtual timer expired",
        nix::libc::SIGPROF => "Profiling timer expired",
        nix::libc::SIGWINCH => "Window changed",
        nix::libc::SIGIO => "I/O possible",
        nix::libc::SIGSYS => "Bad system call",
        _ => return format!("Signal {signal}"),
    };
    text.to_string()
}

/// One bash-compatible job notice. The status field is 27 columns wide in bash
/// 5.3, and a dumped core is marked before the command.
fn job_notice(id: usize, marker: char, status: &str, dumped: bool, command: &str) -> String {
    format!(
        "[{id}]{marker}  {status:<27}{}{command}",
        if dumped { "(core dumped) " } else { "" }
    )
}

fn negate_flow(flow: Flow, negate: bool) -> Flow {
    if !negate {
        return flow;
    }
    match flow {
        Flow::Status(code) => Flow::Status(i32::from(code == 0)),
        other => other,
    }
}

// shortcut: heredoc bodies go through an unlinked temp file instead of a pipe,
// so a large body cannot deadlock before the child starts reading.
fn parse_signal(name: &str) -> Option<Signal> {
    let name = name.trim_start_matches("SIG").to_ascii_uppercase();
    if let Ok(number) = name.parse::<i32>() {
        return Signal::try_from(number).ok();
    }
    match name.as_str() {
        "HUP" => Some(Signal::SIGHUP),
        "INT" => Some(Signal::SIGINT),
        "QUIT" => Some(Signal::SIGQUIT),
        "KILL" => Some(Signal::SIGKILL),
        "TERM" => Some(Signal::SIGTERM),
        "USR1" => Some(Signal::SIGUSR1),
        "USR2" => Some(Signal::SIGUSR2),
        "PIPE" => Some(Signal::SIGPIPE),
        "ALRM" => Some(Signal::SIGALRM),
        "STOP" => Some(Signal::SIGSTOP),
        "CONT" => Some(Signal::SIGCONT),
        "TSTP" => Some(Signal::SIGTSTP),
        _ => None,
    }
}

fn alias_has_control_operator(value: &str) -> bool {
    value.contains('\n') || value.chars().any(|char| matches!(char, '|' | '&' | ';' | '<' | '>'))
}

// ---------------------------------------------------------------------------
// echo and printf
//
// Both are builtins: a host whose `echo` or `printf` differs from bash's
// would otherwise change what a script prints, and every call would cost a
// fork. The behaviour below matches bash 5.3; escapes and numbers follow the
// same rules bash implements, written out here so the output never depends on
// the C library on the machine.
// ---------------------------------------------------------------------------

/// The backslash dialects bash uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Escapes {
    /// `echo -e`: octal needs a leading zero and `\c` stops everything.
    Echo,
    /// `printf %b`: like `echo -e`, except that `\123` is octal too.
    PercentB,
    /// A `printf` format: octal needs no leading zero, `\'`, `\"` and `\?`
    /// lose the backslash, `\c` is ordinary text and `\%` leaves the `%` to
    /// the conversion parser.
    Format,
}

/// One decoded backslash escape.
enum Escape {
    Bytes(Vec<u8>),
    /// A format `\%`: emit the backslash and let the caller reread the `%`.
    Rescan,
    /// `\c`: nothing after it is printed.
    Stop,
}

fn write_output(bytes: &[u8]) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(bytes);
    let _ = stdout.flush();
}

fn echo_builtin(args: &[String]) -> i32 {
    let mut newline = true;
    let mut escapes = false;
    let mut index = 0;
    // Flags are a run of leading words made only of n, e and E. The first word
    // that is not such a word is data, and `-` alone is data too.
    while let Some(word) = args.get(index) {
        let Some(flags) = word.strip_prefix('-') else {
            break;
        };
        if flags.is_empty() || !flags.chars().all(|flag| matches!(flag, 'n' | 'e' | 'E')) {
            break;
        }
        for flag in flags.chars() {
            match flag {
                'n' => newline = false,
                'e' => escapes = true,
                _ => escapes = false,
            }
        }
        index += 1;
    }

    let mut out = Vec::new();
    let mut stopped = false;
    for (position, word) in args[index..].iter().enumerate() {
        if position > 0 {
            out.push(b' ');
        }
        if escapes {
            let (bytes, stop) = decode_escapes(word.as_bytes(), Escapes::Echo, false);
            out.extend_from_slice(&bytes);
            if stop {
                stopped = true;
                break;
            }
        } else {
            out.extend_from_slice(word.as_bytes());
        }
    }
    if newline && !stopped {
        out.push(b'\n');
    }
    write_output(&out);
    0
}

/// Decode every escape in `input`. The second value is true when `\c` ended
/// the output early.
fn decode_escapes(input: &[u8], dialect: Escapes, warn: bool) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'\\' {
            out.push(input[index]);
            index += 1;
            continue;
        }
        match decode_escape(input, index + 1, dialect, warn) {
            (Escape::Bytes(bytes), next) => {
                out.extend_from_slice(&bytes);
                index = next;
            }
            (Escape::Rescan, next) => {
                out.push(b'\\');
                index = next;
            }
            (Escape::Stop, _) => return (out, true),
        }
    }
    (out, false)
}

/// Decode one escape; `index` points at the character after the backslash and
/// the second value is where decoding resumes.
fn decode_escape(input: &[u8], index: usize, dialect: Escapes, warn: bool) -> (Escape, usize) {
    let Some(&escape) = input.get(index) else {
        return (Escape::Bytes(vec![b'\\']), index);
    };
    let mut next = index + 1;
    let bytes = match escape {
        b'\\' => vec![b'\\'],
        b'a' => vec![0x07],
        b'b' => vec![0x08],
        b'e' | b'E' => vec![0x1b],
        b'f' => vec![0x0c],
        b'n' => vec![b'\n'],
        b'r' => vec![b'\r'],
        b't' => vec![b'\t'],
        b'v' => vec![0x0b],
        b'\'' | b'"' | b'?' if dialect == Escapes::Format => vec![escape],
        b'%' if dialect == Escapes::Format => return (Escape::Rescan, index),
        b'c' if dialect != Escapes::Format => return (Escape::Stop, next),
        b'0' if dialect != Escapes::Format => {
            let (value, after) = octal_digits(input, next, 3);
            next = after;
            vec![(value & 0xff) as u8]
        }
        b'0'..=b'7' if dialect == Escapes::PercentB => {
            let mut value = u32::from(escape - b'0');
            let mut taken = 1;
            while taken < 3 {
                let Some(&digit @ b'0'..=b'7') = input.get(next) else {
                    break;
                };
                value = value * 8 + u32::from(digit - b'0');
                next += 1;
                taken += 1;
            }
            vec![(value & 0xff) as u8]
        }
        b'0'..=b'7' if dialect == Escapes::Format => {
            let mut value = u32::from(escape - b'0');
            let mut taken = 1;
            while taken < 3 {
                let Some(&digit @ b'0'..=b'7') = input.get(next) else {
                    break;
                };
                value = value * 8 + u32::from(digit - b'0');
                next += 1;
                taken += 1;
            }
            vec![(value & 0xff) as u8]
        }
        b'x' => {
            let (value, after, taken) = hex_digits(input, next, 2);
            next = after;
            if taken == 0 {
                missing_escape_digit(warn, "hex", 'x');
                vec![b'\\', b'x']
            } else {
                vec![(value & 0xff) as u8]
            }
        }
        b'u' | b'U' => {
            let limit = if escape == b'u' { 4 } else { 8 };
            let (value, after, taken) = hex_digits(input, next, limit);
            next = after;
            if taken == 0 {
                missing_escape_digit(warn, "unicode", escape as char);
                vec![b'\\', escape]
            } else {
                let mut encoded = Vec::new();
                push_utf8(value, &mut encoded);
                encoded
            }
        }
        _ => vec![b'\\', escape],
    };
    (Escape::Bytes(bytes), next)
}

fn missing_escape_digit(warn: bool, kind: &str, letter: char) {
    if warn {
        eprintln!("sharky: printf: missing {kind} digit for \\{letter}");
    }
}

fn octal_digits(input: &[u8], mut index: usize, limit: usize) -> (u32, usize) {
    let mut value = 0;
    let mut taken = 0;
    while taken < limit {
        let Some(&digit @ b'0'..=b'7') = input.get(index) else {
            break;
        };
        value = value * 8 + u32::from(digit - b'0');
        index += 1;
        taken += 1;
    }
    (value, index)
}

fn hex_digits(input: &[u8], mut index: usize, limit: usize) -> (u32, usize, usize) {
    let mut value = 0;
    let mut taken = 0;
    while taken < limit {
        let Some(digit) = input.get(index).and_then(|byte| (*byte as char).to_digit(16)) else {
            break;
        };
        value = value * 16 + digit;
        index += 1;
        taken += 1;
    }
    (value, index, taken)
}

/// Encode a code point the way bash's `\u`/`\U` escapes do: the obsolete five-
/// and six-byte forms are produced for values that need them, and a code point
/// above `0x7fffffff` produces nothing.
fn push_utf8(value: u32, out: &mut Vec<u8>) {
    let byte = |shift: u32, mask: u8| ((value >> shift) as u8 & mask) | 0x80;
    match value {
        0..=0x7f => out.push(value as u8),
        0x80..=0x7ff => {
            out.push(0xc0 | (value >> 6) as u8);
            out.push(byte(0, 0x3f));
        }
        0x800..=0xffff => {
            out.push(0xe0 | (value >> 12) as u8);
            out.push(byte(6, 0x3f));
            out.push(byte(0, 0x3f));
        }
        0x1_0000..=0x1f_ffff => {
            out.push(0xf0 | (value >> 18) as u8);
            out.push(byte(12, 0x3f));
            out.push(byte(6, 0x3f));
            out.push(byte(0, 0x3f));
        }
        0x20_0000..=0x3ff_ffff => {
            out.push(0xf8 | (value >> 24) as u8);
            out.push(byte(18, 0x3f));
            out.push(byte(12, 0x3f));
            out.push(byte(6, 0x3f));
            out.push(byte(0, 0x3f));
        }
        0x400_0000..=0x7fff_ffff => {
            out.push(0xfc | (value >> 30) as u8);
            out.push(byte(24, 0x3f));
            out.push(byte(18, 0x3f));
            out.push(byte(12, 0x3f));
            out.push(byte(6, 0x3f));
            out.push(byte(0, 0x3f));
        }
        _ => {}
    }
}

/// How a format pass ended.
enum FormatFlow {
    /// The format was printed; another pass may follow.
    Done,
    /// `%b` found `\c`: print nothing more.
    Stop,
    /// The format is invalid: stop, and the status becomes 1.
    Failed,
}

enum Conversion {
    Done,
    Stop,
    Failed,
}

/// The printf conversion flags.
#[derive(Default, Clone, Copy)]
struct Flags {
    left: bool,
    plus: bool,
    space: bool,
    alternate: bool,
    zero: bool,
}

impl Shell {
    // printf writes to stdout, or to one variable with -v. Integers, floats and
    // escapes are all formatted here, so the output does not depend on the host
    // C library or on coreutils being the same version as bash.
    fn printf_builtin(&mut self, words: &[String]) -> i32 {
        let mut index = 0;
        let mut target: Option<&str> = None;
        while let Some(word) = words.get(index) {
            if word == "--" {
                index += 1;
                break;
            }
            if !word.starts_with('-') || word == "-" {
                break;
            }
            if word == "-v" {
                match words.get(index + 1) {
                    Some(name) => target = Some(name),
                    None => {
                        printf_usage();
                        return 2;
                    }
                }
                index += 2;
                continue;
            }
            eprintln!("sharky: printf: {word}: invalid option");
            printf_usage();
            return 2;
        }
        let Some(format) = words.get(index) else {
            printf_usage();
            return 2;
        };
        if let Some(name) = target
            && !is_valid_array_name(name)
        {
            eprintln!("sharky: printf: {name}: not a valid identifier");
            return 2;
        }

        let args = words.get(index + 1..).unwrap_or_default();
        let mut out = Vec::new();
        let mut failed = false;
        let mut cursor = 0;
        loop {
            let before = cursor;
            match format_pass(format.as_bytes(), args, &mut cursor, &mut out, &mut failed) {
                FormatFlow::Done => {}
                FormatFlow::Stop | FormatFlow::Failed => break,
            }
            // The format repeats only while it still consumes arguments, so a
            // format with no conversion prints surplus arguments once.
            if cursor >= args.len() || cursor == before {
                break;
            }
        }

        match target {
            Some(name) => {
                let value = String::from_utf8_lossy(&out).into_owned();
                if !self.set_var(name, value) {
                    return 1;
                }
            }
            None => write_output(&out),
        }
        if failed { 1 } else { 0 }
    }
}

fn printf_usage() {
    eprintln!("sharky: printf: usage: printf [-v var] format [arguments]");
}

fn format_pass(
    format: &[u8],
    args: &[String],
    cursor: &mut usize,
    out: &mut Vec<u8>,
    failed: &mut bool,
) -> FormatFlow {
    let mut index = 0;
    while index < format.len() {
        match format[index] {
            b'\\' => match decode_escape(format, index + 1, Escapes::Format, true) {
                (Escape::Bytes(bytes), next) => {
                    out.extend_from_slice(&bytes);
                    index = next;
                }
                (Escape::Rescan, next) => {
                    out.push(b'\\');
                    index = next;
                }
                (Escape::Stop, next) => index = next,
            },
            b'%' => {
                let start = index;
                index += 1;
                match conversion(format, start, &mut index, args, cursor, out, failed) {
                    Conversion::Done => {}
                    Conversion::Stop => return FormatFlow::Stop,
                    Conversion::Failed => return FormatFlow::Failed,
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    FormatFlow::Done
}

/// One `%` conversion. `start` is the index of the `%`; `index` moves past the
/// conversion or stops where the format became invalid.
fn conversion(
    format: &[u8],
    start: usize,
    index: &mut usize,
    args: &[String],
    cursor: &mut usize,
    out: &mut Vec<u8>,
    failed: &mut bool,
) -> Conversion {
    // `%` is only a literal when nothing sits between it and a second `%`:
    // bash rejects `%5%` as an invalid format character.
    if format.get(*index) == Some(&b'%') {
        *index += 1;
        out.push(b'%');
        return Conversion::Done;
    }

    let mut flags = Flags::default();
    while let Some(&byte) = format.get(*index) {
        match byte {
            b'-' => flags.left = true,
            b'+' => flags.plus = true,
            b' ' => flags.space = true,
            b'#' => flags.alternate = true,
            b'0' => flags.zero = true,
            // The grouping flag is accepted and has no effect in a C locale.
            b'\'' => {}
            _ => break,
        }
        *index += 1;
    }

    let mut width: Option<usize> = None;
    if format.get(*index) == Some(&b'*') {
        *index += 1;
        let value = star_argument(args, cursor, failed);
        if value < 0 {
            flags.left = true;
            width = Some(value.unsigned_abs() as usize);
        } else {
            width = Some(value as usize);
        }
    } else {
        let (value, after, taken) = decimal_digits(format, *index);
        if taken > 0 {
            width = Some(value as usize);
            *index = after;
        }
    }

    let mut precision: Option<usize> = None;
    if format.get(*index) == Some(&b'.') {
        *index += 1;
        if format.get(*index) == Some(&b'*') {
            *index += 1;
            let value = star_argument(args, cursor, failed);
            if value >= 0 {
                precision = Some(value as usize);
            }
        } else {
            let (value, after, _) = decimal_digits(format, *index);
            precision = Some(value as usize);
            *index = after;
        }
    }

    // Length modifiers are accepted and ignored, as bash does.
    while matches!(
        format.get(*index),
        Some(b'h' | b'l' | b'L' | b'j' | b'z' | b't')
    ) {
        *index += 1;
    }

    let Some(&letter) = format.get(*index) else {
        *failed = true;
        eprintln!(
            "sharky: printf: `{}': missing format character",
            String::from_utf8_lossy(&format[start..])
        );
        return Conversion::Failed;
    };
    *index += 1;

    match letter {
        b's' => {
            let value = argument(args, cursor).unwrap_or("");
            emit_text(out, value.as_bytes(), precision, &flags, width);
        }
        b'c' => {
            let value = argument(args, cursor).unwrap_or("");
            let first = value.as_bytes().first().copied().unwrap_or(0);
            emit_text(out, &[first], None, &flags, width);
        }
        b'b' => {
            let value = argument(args, cursor).unwrap_or("");
            let (bytes, stopped) = decode_escapes(value.as_bytes(), Escapes::PercentB, true);
            emit_text(out, &bytes, precision, &flags, width);
            if stopped {
                return Conversion::Stop;
            }
        }
        b'q' => {
            let value = argument(args, cursor).unwrap_or("");
            let quoted = quote_for_reuse(value.as_bytes());
            emit_text(out, &quoted, precision, &flags, width);
        }
        b'n' => {
            let _ = argument(args, cursor);
        }
        b'd' | b'i' => {
            let value = signed_argument(args, cursor, failed);
            let digits = value.unsigned_abs().to_string();
            let prefix = if value < 0 {
                "-"
            } else if flags.plus {
                "+"
            } else if flags.space {
                " "
            } else {
                ""
            };
            emit_number(out, prefix, &digits, precision, false, &flags, width);
        }
        b'u' | b'o' | b'x' | b'X' => {
            let value = unsigned_argument(args, cursor, failed);
            let mut digits = match letter {
                b'u' => value.to_string(),
                b'o' => format!("{value:o}"),
                b'x' => format!("{value:x}"),
                _ => format!("{value:X}"),
            };
            let octal_alternate = letter == b'o' && flags.alternate;
            if octal_alternate && !digits.starts_with('0') {
                digits.insert(0, '0');
            }
            let prefix = match letter {
                b'x' if flags.alternate && value != 0 => "0x",
                b'X' if flags.alternate && value != 0 => "0X",
                _ => "",
            };
            emit_number(out, prefix, &digits, precision, octal_alternate, &flags, width);
        }
        b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
            let value = float_argument(args, cursor, failed);
            let upper = matches!(letter, b'F' | b'E' | b'G');
            let prefix = if value.is_sign_negative() {
                "-"
            } else if flags.plus {
                "+"
            } else if flags.space {
                " "
            } else {
                ""
            };
            let mut body = match letter {
                b'f' | b'F' => format_fixed(value.abs(), precision.unwrap_or(6), upper),
                b'e' | b'E' => {
                    format_exponential(value.abs(), precision.unwrap_or(6), upper)
                }
                _ => format_general(value.abs(), precision, flags.alternate, upper),
            };
            // `#` keeps a decimal point even when no fraction digit is printed.
            if flags.alternate && value.is_finite() && !body.contains('.') {
                let at = body.find(['e', 'E']).unwrap_or(body.len());
                body.insert(at, '.');
            }
            emit_float(out, prefix, &body, &flags, width, value.is_finite());
        }
        _ => {
            *failed = true;
            eprintln!(
                "sharky: printf: `{}': invalid format character",
                String::from_utf8_lossy(&format[*index - 1..*index])
            );
            return Conversion::Failed;
        }
    }
    Conversion::Done
}

fn argument<'a>(args: &'a [String], cursor: &mut usize) -> Option<&'a str> {
    let value = args.get(*cursor).map(String::as_str);
    if value.is_some() {
        *cursor += 1;
    }
    value
}

fn emit_text(
    out: &mut Vec<u8>,
    body: &[u8],
    precision: Option<usize>,
    flags: &Flags,
    width: Option<usize>,
) {
    let body = match precision {
        Some(limit) if body.len() > limit => &body[..limit],
        _ => body,
    };
    let padding = width.unwrap_or(0).saturating_sub(body.len());
    if flags.left {
        out.extend_from_slice(body);
        out.extend(std::iter::repeat_n(b' ', padding));
    } else {
        out.extend(std::iter::repeat_n(b' ', padding));
        out.extend_from_slice(body);
    }
}

fn emit_number(
    out: &mut Vec<u8>,
    prefix: &str,
    digits: &str,
    precision: Option<usize>,
    keep_zero: bool,
    flags: &Flags,
    width: Option<usize>,
) {
    let mut digits = digits.to_owned();
    if let Some(limit) = precision {
        if digits.len() < limit {
            digits.insert_str(0, &"0".repeat(limit - digits.len()));
        } else if limit == 0 && digits == "0" && !keep_zero {
            digits.clear();
        }
    }
    let padding = width
        .unwrap_or(0)
        .saturating_sub(prefix.len() + digits.len());
    if flags.left {
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(digits.as_bytes());
        out.extend(std::iter::repeat_n(b' ', padding));
    } else if flags.zero && precision.is_none() {
        out.extend_from_slice(prefix.as_bytes());
        out.extend(std::iter::repeat_n(b'0', padding));
        out.extend_from_slice(digits.as_bytes());
    } else {
        out.extend(std::iter::repeat_n(b' ', padding));
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(digits.as_bytes());
    }
}

fn emit_float(
    out: &mut Vec<u8>,
    prefix: &str,
    body: &str,
    flags: &Flags,
    width: Option<usize>,
    zero_pads: bool,
) {
    let padding = width
        .unwrap_or(0)
        .saturating_sub(prefix.len() + body.len());
    if flags.left {
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(body.as_bytes());
        out.extend(std::iter::repeat_n(b' ', padding));
    } else if flags.zero && zero_pads {
        out.extend_from_slice(prefix.as_bytes());
        out.extend(std::iter::repeat_n(b'0', padding));
        out.extend_from_slice(body.as_bytes());
    } else {
        out.extend(std::iter::repeat_n(b' ', padding));
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(body.as_bytes());
    }
}

fn non_finite(value: f64, upper: bool) -> String {
    let text = if value.is_nan() { "nan" } else { "inf" };
    if upper {
        text.to_ascii_uppercase()
    } else {
        text.to_owned()
    }
}

fn format_fixed(value: f64, precision: usize, upper: bool) -> String {
    if !value.is_finite() {
        return non_finite(value, upper);
    }
    format!("{value:.precision$}")
}

fn format_exponential(value: f64, precision: usize, upper: bool) -> String {
    if !value.is_finite() {
        return non_finite(value, upper);
    }
    let text = format!("{value:.precision$e}");
    let (mantissa, exponent) = text.split_once('e').unwrap_or((text.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let marker = if upper { 'E' } else { 'e' };
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}{marker}{sign}{:02}", exponent.abs())
}

/// `%g`: the shorter of `%e` and `%f` for the requested number of significant
/// digits, with trailing fractional zeros removed unless `#` was given.
fn format_general(value: f64, precision: Option<usize>, alternate: bool, upper: bool) -> String {
    if !value.is_finite() {
        return non_finite(value, upper);
    }
    let significant = precision.unwrap_or(6).max(1);
    let scientific = format!("{value:.*e}", significant - 1);
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let marker = if upper { 'E' } else { 'e' };
    let exponent_text = format!(
        "{}{:02}",
        if exponent < 0 { '-' } else { '+' },
        exponent.abs()
    );

    if exponent < -4 || exponent >= significant as i32 {
        if alternate {
            return format!("{mantissa}{marker}{exponent_text}");
        }
        let trimmed = trim_fraction(mantissa);
        return format!("{trimmed}{marker}{exponent_text}");
    }

    let decimals = (significant as i32 - 1 - exponent).max(0) as usize;
    let fixed = format!("{value:.decimals$}");
    if alternate {
        return if fixed.contains('.') {
            fixed
        } else {
            format!("{fixed}.")
        };
    }
    trim_fraction(&fixed)
}

/// Drop trailing zeros after a decimal point, and the point itself when
/// nothing is left after it. Digits before the point are never touched.
fn trim_fraction(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    let trimmed = text.trim_end_matches('0');
    trimmed.strip_suffix('.').unwrap_or(trimmed).to_owned()
}

/// Quote a value so it reads back unchanged, the way bash's `%q` does.
fn quote_for_reuse(value: &[u8]) -> Vec<u8> {
    if value.is_empty() {
        return b"''".to_vec();
    }
    if value.iter().any(|byte| is_control_byte(*byte)) {
        let mut out = b"$'".to_vec();
        for &byte in value {
            match byte {
                0x07 => out.extend_from_slice(b"\\a"),
                0x08 => out.extend_from_slice(b"\\b"),
                0x09 => out.extend_from_slice(b"\\t"),
                0x0a => out.extend_from_slice(b"\\n"),
                0x0b => out.extend_from_slice(b"\\v"),
                0x0c => out.extend_from_slice(b"\\f"),
                0x0d => out.extend_from_slice(b"\\r"),
                0x1b => out.extend_from_slice(b"\\E"),
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\'' => out.extend_from_slice(b"\\'"),
                byte if is_control_byte(byte) => {
                    out.extend_from_slice(format!("\\{byte:03o}").as_bytes());
                }
                byte => out.push(byte),
            }
        }
        out.push(b'\'');
        return out;
    }

    let mut out = Vec::with_capacity(value.len());
    for (position, &byte) in value.iter().enumerate() {
        // `~` and `#` only mean something at the start of a word.
        let safe = is_reusable_byte(byte) || (position > 0 && matches!(byte, b'~' | b'#'));
        if !safe {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out
}

fn is_control_byte(byte: u8) -> bool {
    byte < 0x20 || byte == 0x7f
}

/// Bytes bash's `%q` leaves bare: the ASCII set it measured plus every
/// non-ASCII byte, which the C.UTF-8 locale treats as printable.
fn is_reusable_byte(byte: u8) -> bool {
    byte >= 0x80
        || byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'%' | b'+' | b'-' | b'.' | b'/' | b':' | b'=' | b'@' | b'_'
        )
}

/// What `strtoimax` would not tell us: whether anything at all was a number.
struct IntegerScan {
    negative: bool,
    digits: bool,
    value: u128,
    junk: bool,
    /// The text began with `0x` but held no hex digit.
    bad_hex: bool,
}

/// bash's integer argument rules: optional blanks and sign, a `'` or `"` that
/// takes the next character's code point (and ignores the rest), then a base-0
/// number, which also accepts a `0b` binary prefix.
fn scan_integer(text: &str) -> IntegerScan {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let mut negative = false;
    if let Some(&byte) = bytes.get(index) {
        if byte == b'+' || byte == b'-' {
            negative = byte == b'-';
            index += 1;
        }
    }

    if let Some(&quote) = bytes.get(index)
        && (quote == b'\'' || quote == b'"')
    {
        let rest = text.get(index + 1..).unwrap_or_default();
        let value = rest.chars().next().map_or(0, |character| u32::from(character) as u128);
        return IntegerScan {
            negative,
            digits: true,
            value,
            junk: false,
            bad_hex: false,
        };
    }

    let digit_at = |position: usize, radix: u32| {
        bytes
            .get(position)
            .and_then(|byte| (*byte as char).to_digit(radix))
    };
    let (radix, start, bad_hex) = if bytes.get(index) == Some(&b'0')
        && matches!(bytes.get(index + 1), Some(b'x' | b'X'))
    {
        if digit_at(index + 2, 16).is_some() {
            (16, index + 2, false)
        } else {
            (8, index, true)
        }
    } else if bytes.get(index) == Some(&b'0')
        && matches!(bytes.get(index + 1), Some(b'b' | b'B'))
        && digit_at(index + 2, 2).is_some()
    {
        (2, index + 2, false)
    } else if bytes.get(index) == Some(&b'0') {
        (8, index, false)
    } else {
        (10, index, false)
    };
    let mut value: u128 = 0;
    let mut digits = false;
    let mut position = start;
    while let Some(digit) = digit_at(position, radix) {
        value = value
            .saturating_mul(u128::from(radix))
            .saturating_add(u128::from(digit));
        digits = true;
        position += 1;
    }
    // bash rejects even trailing blanks here: `5 ` is an invalid number while
    // ` 5` is fine.
    let junk = position != text.len();
    IntegerScan {
        negative,
        digits,
        value,
        junk,
        bad_hex,
    }
}

fn signed_argument(args: &[String], cursor: &mut usize, failed: &mut bool) -> i64 {
    let Some(text) = argument(args, cursor) else {
        return 0;
    };
    let scan = scan_integer(text);
    if let Some(problem) = integer_problem(&scan) {
        *failed = true;
        number_error(text, problem);
        if !scan.digits {
            return 0;
        }
    }
    let limit = if scan.negative {
        1u128 << 63
    } else {
        (1u128 << 63) - 1
    };
    if scan.value > limit {
        *failed = true;
        number_error(text, "Numerical result out of range");
        return if scan.negative { i64::MIN } else { i64::MAX };
    }
    let value = scan.value as i128;
    (if scan.negative { -value } else { value }) as i64
}

fn unsigned_argument(args: &[String], cursor: &mut usize, failed: &mut bool) -> u64 {
    let Some(text) = argument(args, cursor) else {
        return 0;
    };
    let scan = scan_integer(text);
    if let Some(problem) = integer_problem(&scan) {
        *failed = true;
        number_error(text, problem);
        if !scan.digits {
            return 0;
        }
    }
    if scan.value > u128::from(u64::MAX) {
        *failed = true;
        number_error(text, "Numerical result out of range");
        return u64::MAX;
    }
    let value = scan.value as u64;
    if scan.negative { value.wrapping_neg() } else { value }
}

fn star_argument(args: &[String], cursor: &mut usize, failed: &mut bool) -> i64 {
    let Some(text) = argument(args, cursor) else {
        return 0;
    };
    let scan = scan_integer(text);
    if let Some(problem) = integer_problem(&scan) {
        *failed = true;
        number_error(text, problem);
    }
    let value = scan.value.min(i64::MAX as u128) as i64;
    if scan.negative { -value } else { value }
}

fn integer_problem(scan: &IntegerScan) -> Option<&'static str> {
    if scan.bad_hex {
        return Some("invalid hex number");
    }
    if !scan.digits || scan.junk {
        return Some("invalid number");
    }
    None
}

fn float_argument(args: &[String], cursor: &mut usize, failed: &mut bool) -> f64 {
    let Some(text) = argument(args, cursor) else {
        return 0.0;
    };
    let (value, problem) = parse_float(text);
    if let Some(message) = problem {
        *failed = true;
        number_error(text, message);
    }
    value
}

fn number_error(text: &str, message: &str) {
    eprintln!("sharky: printf: {text}: {message}");
}

/// bash's float arguments: `strtod` spellings, including `inf`, `nan` and
/// hexadecimal floats, with any trailing byte making the whole argument
/// invalid.
fn parse_float(text: &str) -> (f64, Option<&'static str>) {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let mut negative = false;
    if let Some(&byte) = bytes.get(index) {
        if byte == b'+' || byte == b'-' {
            negative = byte == b'-';
            index += 1;
        }
    }
    let rest = text.get(index..).unwrap_or_default();
    let lower = rest.to_ascii_lowercase();

    if lower.starts_with("inf") || lower.starts_with("nan") {
        let (consumed, value) = if lower.starts_with("infinity") {
            (8, f64::INFINITY)
        } else if lower.starts_with("inf") {
            (3, f64::INFINITY)
        } else {
            (3, f64::NAN)
        };
        let value = if negative { -value } else { value };
        let junk = rest.len() != consumed;
        return (value, if junk { Some("invalid number") } else { None });
    }

    if lower.starts_with("0x") {
        let (value, problem) = parse_hex_float(rest);
        return (if negative { -value } else { value }, problem);
    }

    let plain = rest.as_bytes();
    let mut position = 0;
    let mut digits = false;
    while plain.get(position).is_some_and(u8::is_ascii_digit) {
        position += 1;
        digits = true;
    }
    if plain.get(position) == Some(&b'.') {
        let mut after = position + 1;
        let mut fraction = false;
        while plain.get(after).is_some_and(u8::is_ascii_digit) {
            after += 1;
            fraction = true;
        }
        if digits || fraction {
            position = after;
            digits = true;
        }
    }
    if !digits {
        return (0.0, Some("invalid number"));
    }
    if matches!(plain.get(position), Some(b'e' | b'E')) {
        let mut after = position + 1;
        if matches!(plain.get(after), Some(b'+' | b'-')) {
            after += 1;
        }
        let start = after;
        while plain.get(after).is_some_and(u8::is_ascii_digit) {
            after += 1;
        }
        if after > start {
            position = after;
        }
    }
    let head = rest.get(..position).unwrap_or_default();
    let value: f64 = head.parse().unwrap_or(0.0);
    let value = if negative { -value } else { value };
    (value, if position == rest.len() { None } else { Some("invalid number") })
}

fn parse_hex_float(rest: &str) -> (f64, Option<&'static str>) {
    let bytes = rest.as_bytes();
    let mut position = 2;
    let mut mantissa = 0.0;
    let mut exponent = 0i32;
    let mut digits = false;
    while let Some(digit) = bytes
        .get(position)
        .and_then(|byte| (*byte as char).to_digit(16))
    {
        mantissa = mantissa * 16.0 + f64::from(digit);
        position += 1;
        digits = true;
    }
    if bytes.get(position) == Some(&b'.') {
        let mut after = position + 1;
        while let Some(digit) = bytes
            .get(after)
            .and_then(|byte| (*byte as char).to_digit(16))
        {
            mantissa = mantissa * 16.0 + f64::from(digit);
            exponent -= 4;
            after += 1;
            digits = true;
        }
        position = after;
    }
    if !digits {
        return (0.0, Some("invalid hex number"));
    }
    if matches!(bytes.get(position), Some(b'p' | b'P')) {
        let mut after = position + 1;
        let mut negative = false;
        if matches!(bytes.get(after), Some(b'+' | b'-')) {
            negative = bytes[after] == b'-';
            after += 1;
        }
        let start = after;
        let mut power = 0i32;
        while let Some(digit) = bytes
            .get(after)
            .and_then(|byte| (*byte as char).to_digit(10))
        {
            power = power.saturating_mul(10).saturating_add(digit as i32);
            after += 1;
        }
        if after > start {
            exponent = exponent.saturating_add(if negative { -power } else { power });
            position = after;
        }
    }
    let value = mantissa * 2f64.powi(exponent);
    (
        value,
        if position == bytes.len() {
            None
        } else {
            Some("invalid number")
        },
    )
}

fn decimal_digits(input: &[u8], mut index: usize) -> (u64, usize, usize) {
    let mut value: u64 = 0;
    let mut taken = 0;
    while let Some(&digit @ b'0'..=b'9') = input.get(index) {
        value = value.saturating_mul(10).saturating_add(u64::from(digit - b'0'));
        index += 1;
        taken += 1;
    }
    (value, index, taken)
}

/// The one leading option word bash accepts for `eval` and `getopts`: `--` is
/// skipped, `--help` prints the topic and stops with status 2, and any other
/// word starting with `-` is rejected with the usage line. Returns the index of
/// the first operand.
fn skip_leading_options(name: &str, usage: &str, args: &[String]) -> Result<usize, i32> {
    match args.first().map(String::as_str) {
        Some("--") => Ok(1),
        Some("--help") => {
            if let Some(text) = crate::help::topic(name) {
                print!("{text}");
            }
            Err(2)
        }
        Some(arg) if arg.len() > 1 && arg.starts_with('-') => {
            eprintln!("sharky: {name}: {arg}: invalid option");
            eprintln!("{usage}");
            Err(2)
        }
        _ => Ok(0),
    }
}

/// bash's `read_octal`: at most four octal digits, none above 7, and the value
/// must stay within 07777. Anything else is reported as out of range.
fn read_octal(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    let mut value: u32 = 0;
    let mut digits = 0;
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() && bytes[index] <= b'7' {
        value = value * 8 + u32::from(bytes[index] - b'0');
        if value > 0o7777 {
            return None;
        }
        digits += 1;
        index += 1;
    }
    if digits == 0 || index != bytes.len() {
        return None;
    }
    Some(value)
}

/// The process umask. Reading it needs a write, so the previous value is put
/// back immediately; callers get the same state they had.
fn umask_current() -> u32 {
    // SAFETY: umask(2) touches only the process attribute, and the previous
    // value is restored before anything else can observe the change.
    let current = unsafe { nix::libc::umask(0) };
    unsafe { nix::libc::umask(current) };
    (current as u32) & 0o777
}

fn umask_set(bits: u32) {
    // SAFETY: the value is masked to the nine mode bits the kernel stores.
    unsafe { nix::libc::umask(bits as nix::libc::mode_t) };
}

/// `u=rwx,g=rx,o=` — a letter printed when the mask leaves that bit clear.
fn umask_symbolic(mask: u32) -> String {
    let class = |read: u32, write: u32, execute: u32| {
        let mut out = String::new();
        if mask & read == 0 {
            out.push('r');
        }
        if mask & write == 0 {
            out.push('w');
        }
        if mask & execute == 0 {
            out.push('x');
        }
        out
    };
    format!(
        "u={},g={},o={}",
        class(0o400, 0o200, 0o100),
        class(0o040, 0o020, 0o010),
        class(0o004, 0o002, 0o001)
    )
}

const UMASK_READ_ALL: u32 = 0o444;
const UMASK_WRITE_ALL: u32 = 0o222;
const UMASK_EXECUTE_ALL: u32 = 0o111;
const UMASK_ALL: u32 = 0o777;

fn umask_copy_user(bits: u32) -> u32 {
    (if bits & 0o400 != 0 { UMASK_READ_ALL } else { 0 })
        | (if bits & 0o200 != 0 { UMASK_WRITE_ALL } else { 0 })
        | (if bits & 0o100 != 0 { UMASK_EXECUTE_ALL } else { 0 })
}

fn umask_copy_group(bits: u32) -> u32 {
    (if bits & 0o040 != 0 { UMASK_READ_ALL } else { 0 })
        | (if bits & 0o020 != 0 { UMASK_WRITE_ALL } else { 0 })
        | (if bits & 0o010 != 0 { UMASK_EXECUTE_ALL } else { 0 })
}

fn umask_copy_other(bits: u32) -> u32 {
    (if bits & 0o004 != 0 { UMASK_READ_ALL } else { 0 })
        | (if bits & 0o002 != 0 { UMASK_WRITE_ALL } else { 0 })
        | (if bits & 0o001 != 0 { UMASK_EXECUTE_ALL } else { 0 })
}

/// bash's `parse_symbolic_mode`, run on the permissions the current mask
/// allows: `who` accumulates across a clause, an omitted `who` means `a`, an
/// action may name permissions, copy another class, or start another operator.
/// The search bit `X` applies only when the initial mode already allows one.
fn umask_parse_symbolic(text: &str, initial: u32) -> Option<u32> {
    let bytes = text.as_bytes();
    let mut bits = initial;
    let mut index = 0;
    let mut who: u32 = 0;
    // A comma starts a new clause and re-parses `who`; a second operator
    // continues the clause and keeps the `who` already read.
    let mut fresh_clause = true;

    loop {
        if fresh_clause {
            who = 0;
            while index < bytes.len() {
                match bytes[index] {
                    b'u' => who |= 0o700,
                    b'g' => who |= 0o070,
                    b'o' => who |= 0o007,
                    b'a' => who |= UMASK_ALL,
                    _ => break,
                }
                index += 1;
            }
            if who == 0 {
                who = UMASK_ALL;
            }
        }

        // The operator sits at the cursor; index past it even at the end, the
        // way bash reads a NUL and reports it as invalid.
        let op = bytes.get(index).copied().unwrap_or(0);
        index += 1;
        if !matches!(op, b'+' | b'-' | b'=') {
            eprintln!(
                "sharky: umask: `{}': invalid symbolic mode operator",
                op as char
            );
            return None;
        }

        let mut perm: u32 = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'u' => perm = umask_copy_user(initial),
                b'g' => perm = umask_copy_group(initial),
                b'o' => perm = umask_copy_other(initial),
                b'r' => perm |= UMASK_READ_ALL,
                b'w' => perm |= UMASK_WRITE_ALL,
                b'x' => perm |= UMASK_EXECUTE_ALL,
                // bash tests the initial bits, not the bits built so far.
                b'X' => {
                    if initial & UMASK_EXECUTE_ALL != 0 {
                        perm |= UMASK_EXECUTE_ALL;
                    }
                }
                b's' => perm |= 0o6000,
                b't' => perm |= 0o1000,
                _ => break,
            }
            index += 1;
        }

        // Setuid, setgid and sticky never reach the mask: `who` has no bit for
        // them, so they drop out here exactly as they do in bash.
        perm &= who;
        match op {
            b'+' => bits |= perm,
            b'-' => bits &= !perm,
            _ => {
                bits &= !who;
                bits |= perm;
            }
        }

        if index >= bytes.len() {
            break;
        }
        match bytes[index] {
            b',' => {
                index += 1;
                fresh_clause = true;
            }
            b'+' | b'-' | b'=' => {
                fresh_clause = false;
            }
            other => {
                eprintln!(
                    "sharky: umask: `{}': invalid symbolic mode character",
                    other as char
                );
                return None;
            }
        }
    }

    Some(bits & 0o777)
}

/// `atoi`: leading whitespace, an optional sign, then digits, and 0 when none
/// are found. `getopts` reads OPTIND and OPTERR this way.
fn atoi_like(text: &str) -> i64 {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let mut negative = false;
    if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
        negative = bytes[index] == b'-';
        index += 1;
    }
    let mut value: i64 = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(bytes[index] - b'0'));
        index += 1;
    }
    if negative { -value } else { value }
}

#[derive(Clone, Copy)]
enum LimitKind {
    Blocks(Resource),
    Kbytes(Resource),
    Count(Resource),
    Seconds(Resource),
}

impl LimitKind {
    fn resource(self) -> Resource {
        match self {
            Self::Blocks(resource)
            | Self::Kbytes(resource)
            | Self::Count(resource)
            | Self::Seconds(resource) => resource,
        }
    }

    fn format(self, value: nix::sys::resource::rlim_t) -> String {
        if value == RLIM_INFINITY {
            return "unlimited".to_string();
        }
        match self {
            Self::Blocks(_) => (value / 512).to_string(),
            Self::Kbytes(_) => (value / 1024).to_string(),
            Self::Count(_) | Self::Seconds(_) => value.to_string(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Blocks(Resource::RLIMIT_CORE) => "core file size",
            Self::Blocks(Resource::RLIMIT_FSIZE) => "file size",
            Self::Count(Resource::RLIMIT_NOFILE) => "open files",
            Self::Count(Resource::RLIMIT_NPROC) => "max user processes",
            Self::Kbytes(Resource::RLIMIT_STACK) => "stack size",
            Self::Kbytes(Resource::RLIMIT_AS) => "virtual memory",
            Self::Seconds(Resource::RLIMIT_CPU) => "cpu time",
            _ => "resource",
        }
    }
}

const ALL_LIMITS: &[(&str, LimitKind)] = &[
    ("core file size              (blocks, -c)", LimitKind::Blocks(Resource::RLIMIT_CORE)),
    ("file size                   (blocks, -f)", LimitKind::Blocks(Resource::RLIMIT_FSIZE)),
    ("open files                          (-n)", LimitKind::Count(Resource::RLIMIT_NOFILE)),
    ("stack size                  (kbytes, -s)", LimitKind::Kbytes(Resource::RLIMIT_STACK)),
    ("cpu time                   (seconds, -t)", LimitKind::Seconds(Resource::RLIMIT_CPU)),
    ("max user processes                  (-u)", LimitKind::Count(Resource::RLIMIT_NPROC)),
    ("virtual memory              (kbytes, -v)", LimitKind::Kbytes(Resource::RLIMIT_AS)),
];

fn parse_stack_index(spec: &str) -> Option<usize> {
    spec[1..].parse().ok()
}

fn times_builtin() -> i32 {
    let format = |who| {
        match getrusage(who) {
            Ok(usage) => {
                let user = usage.user_time();
                let system = usage.system_time();
                format!(
                    "{}m{:02}.{:03}s {}m{:02}.{:03}s",
                    user.tv_sec() / 60,
                    user.tv_sec() % 60,
                    user.tv_usec() / 1000,
                    system.tv_sec() / 60,
                    system.tv_sec() % 60,
                    system.tv_usec() / 1000,
                )
            }
            Err(err) => {
                eprintln!("sharky: times: {err}");
                String::new()
            }
        }
    };
    let shell = format(UsageWho::RUSAGE_SELF);
    let children = format(UsageWho::RUSAGE_CHILDREN);
    if shell.is_empty() || children.is_empty() {
        return 1;
    }
    println!("{shell}");
    println!("{children}");
    0
}

fn hash_builtin(shell: &mut Shell, args: &[String]) -> i32 {
    let mut reset = false;
    let mut list = false;
    let mut print = false;
    let mut delete = false;
    let mut add: Option<(String, std::path::PathBuf)> = None;
    let mut names = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            names.extend(args[index + 1..].iter().cloned());
            break;
        }
        let Some(body) = arg.strip_prefix('-') else {
            names.push(arg.clone());
            index += 1;
            continue;
        };
        if body.is_empty() {
            names.push(arg.clone());
            index += 1;
            continue;
        }
        for flag in body.chars() {
            match flag {
                'r' => reset = true,
                'l' => list = true,
                't' => print = true,
                'd' => delete = true,
                'p' => {
                    let path = args.get(index + 1).cloned();
                    let name = args.get(index + 2).cloned();
                    if path.is_none() || name.is_none() {
                        eprintln!("sharky: hash: -p: option requires a path and a name");
                        return 2;
                    }
                    add = Some((
                        name.as_deref().unwrap().to_string(),
                        std::path::PathBuf::from(path.as_deref().unwrap()),
                    ));
                    index += 2;
                }
                _ => {
                    eprintln!("sharky: hash: -{flag}: invalid option");
                    eprintln!("hash: usage: hash [-lr] [-p path] [-dt] [name ...]");
                    return 2;
                }
            }
        }
        index += 1;
    }
    if reset {
        shell.command_cache.clear();
        shell.highlight_cache.borrow_mut().clear();
    }
    if let Some((name, path)) = add {
        exec::hash_add(&mut shell.command_cache, &name, &path);
        shell.highlight_cache.borrow_mut().remove(&name);
        return 0;
    }
    if delete {
        let mut status = 0;
        for name in &names {
            if !exec::hash_remove(&mut shell.command_cache, name) {
                eprintln!("sharky: hash: {name}: not found");
                status = 1;
            } else {
                shell.highlight_cache.borrow_mut().remove(name);
            }
        }
        return status;
    }
    if print {
        let mut status = 0;
        for name in &names {
            match exec::hashed_path(&shell.command_cache, name) {
                Some(path) => {
                    println!("{}", path.display());
                }
                None => {
                    eprintln!("sharky: hash: {name}: not found");
                    status = 1;
                }
            }
        }
        return status;
    }
    if !names.is_empty() {
        let mut status = 0;
        for name in &names {
            match exec::resolve(name, shell) {
                Some(_) => {}
                None => {
                    eprintln!("sharky: hash: {name}: not found");
                    status = 1;
                }
            }
        }
        return status;
    }
    let entries = exec::hashed(&shell.command_cache);
    if entries.is_empty() {
        if !reset {
            eprintln!("sharky: hash: hash table empty");
            return 1;
        }
        return 0;
    }
    for (name, path) in entries {
        if list {
            println!(
                "builtin hash -p {} {}",
                path.display(),
                name
            );
        } else {
            println!("{}  {}", path.display(), name);
        }
    }
    0
}

impl Shell {
    pub(crate) fn run_hook(&mut self, hook_name: &str, args: &[String]) {
        if self.hook_running {
            return;
        }
        let mut to_run = Vec::new();
        if self.funcs.contains_key(hook_name) {
            to_run.push(hook_name.to_string());
        }
        let arr_name = format!("{hook_name}_functions");
        if let Some(items) = self.arrays.get(&arr_name) {
            for item in items.iter().flatten() {
                if !item.is_empty() && !to_run.contains(item) {
                    to_run.push(item.clone());
                }
            }
        }
        if to_run.is_empty() {
            return;
        }

        self.hook_running = true;
        let saved_status = self.last_status;
        let saved_positional = self.positional.clone();
        for fname in to_run {
            if let Some(body) = self.funcs.get(&fname).cloned() {
                let _ = self.invoke_function(&fname, &body, args);
            }
        }
        self.positional = saved_positional;
        if matches!(hook_name, "precmd" | "chpwd") {
            self.last_status = saved_status;
        }
        self.hook_running = false;
    }

    fn record_dir_history(&mut self) {
        if self.dir_history.get(self.dir_history_index) == Some(&self.cwd) {
            return;
        }
        self.dir_history.truncate(self.dir_history_index + 1);
        self.dir_history.push(self.cwd.clone());
        if self.dir_history.len() > 64 {
            self.dir_history.remove(0);
        }
        self.dir_history_index = self.dir_history.len().saturating_sub(1);
    }

    fn move_dir_history(&mut self, target_index: usize) -> bool {
        let Some(dest) = self.dir_history.get(target_index).cloned() else {
            return false;
        };
        if std::env::set_current_dir(&dest).is_err() {
            return false;
        }
        let previous = self.cwd.clone();
        self.cwd = dest;
        self.dir_history_index = target_index;
        self.set_var("OLDPWD", previous.display().to_string());
        self.set_var("PWD", self.cwd.display().to_string());
        self.run_hook("chpwd", &[]);
        true
    }

    pub(crate) fn dir_back(&mut self) -> bool {
        if self.dir_history_index == 0 {
            return false;
        }
        self.move_dir_history(self.dir_history_index - 1)
    }

    pub(crate) fn dir_forward(&mut self) -> bool {
        if self.dir_history_index + 1 >= self.dir_history.len() {
            return false;
        }
        self.move_dir_history(self.dir_history_index + 1)
    }

    fn prevd_builtin(&mut self, args: &[String]) -> i32 {
        let count = args
            .first()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let mut moved = false;
        for _ in 0..count {
            if !self.dir_back() {
                return if moved { 0 } else { 1 };
            }
            moved = true;
        }
        0
    }

    fn nextd_builtin(&mut self, args: &[String]) -> i32 {
        let count = args
            .first()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let mut moved = false;
        for _ in 0..count {
            if !self.dir_forward() {
                return if moved { 0 } else { 1 };
            }
            moved = true;
        }
        0
    }

    fn parse_completion_args(
        args: &[String],
        command_name: &str,
        allow_complete_flags: bool,
    ) -> Result<(CompletionSpec, bool, bool, Vec<String>), i32> {
        let mut spec = CompletionSpec::default();
        let mut print = false;
        let mut remove = false;
        let mut idx = 0;
        while idx < args.len() {
            let arg = args[idx].as_str();
            if arg == "--" {
                idx += 1;
                break;
            }
            if !arg.starts_with('-') || arg == "-" {
                break;
            }
            for ch in arg.chars().skip(1) {
                match ch {
                    'p' if allow_complete_flags => print = true,
                    'r' if allow_complete_flags => remove = true,
                    'W' => {
                        idx += 1;
                        let Some(words) = args.get(idx) else {
                            eprintln!("sharky: {command_name}: -W: option requires an argument");
                            return Err(2);
                        };
                        spec.wordlist.extend(words.split_whitespace().map(str::to_string));
                    }
                    'F' => {
                        idx += 1;
                        let Some(func) = args.get(idx) else {
                            eprintln!("sharky: {command_name}: -F: option requires an argument");
                            return Err(2);
                        };
                        spec.function = Some(func.clone());
                    }
                    'A' => {
                        idx += 1;
                        let Some(action) = args.get(idx) else {
                            eprintln!("sharky: {command_name}: -A: option requires an argument");
                            return Err(2);
                        };
                        spec.actions.push(action.clone());
                    }
                    'f' => spec.actions.push(String::from("file")),
                    'd' => spec.actions.push(String::from("directory")),
                    'c' => spec.actions.push(String::from("command")),
                    'v' => spec.actions.push(String::from("variable")),
                    'b' => spec.actions.push(String::from("builtin")),
                    'a' => spec.actions.push(String::from("alias")),
                    other => {
                        eprintln!("sharky: {command_name}: -{other}: invalid option");
                        return Err(2);
                    }
                }
            }
            idx += 1;
        }
        Ok((spec, print, remove, args[idx..].to_vec()))
    }

    fn generate_completion_candidates(
        &mut self,
        spec: &CompletionSpec,
        cmd_name: &str,
        words: &[String],
        cword: usize,
        prefix: &str,
        split_descriptions: bool,
    ) -> Vec<(String, Option<String>)> {
        let mut out: Vec<(String, Option<String>)> = Vec::new();
        for item in &spec.wordlist {
            if split_descriptions && let Some((cand, desc)) = item.split_once("::") {
                if cand.starts_with(prefix) {
                    out.push((cand.to_string(), Some(desc.to_string())));
                }
            } else if item.starts_with(prefix) {
                out.push((item.clone(), None));
            }
        }
        if let Some(func_name) = &spec.function
            && let Some(body) = self.funcs.get(func_name).cloned()
        {
            self.set_array("COMP_WORDS", words.to_vec(), false);
            self.set_var("COMP_CWORD", cword.to_string());
            self.set_array("COMPREPLY", Vec::new(), false);
            let prev = words
                .get(cword.saturating_sub(1))
                .cloned()
                .unwrap_or_default();
            let func_args = vec![cmd_name.to_string(), prefix.to_string(), prev];
            let saved_status = self.last_status;
            let _ = self.invoke_function(func_name, &body, &func_args);
            self.last_status = saved_status;
            if let Some(replies) = self.arrays.get("COMPREPLY") {
                for reply in replies.iter().flatten() {
                    if reply.starts_with(prefix) {
                        out.push((reply.clone(), None));
                    }
                }
            }
        }
        for action in &spec.actions {
            match action.as_str() {
                "builtin" => {
                    for &name in crate::help::BUILTINS {
                        if name.starts_with(prefix) {
                            let desc = crate::help::builtin_summary(name).map(str::to_string);
                            out.push((name.to_string(), desc));
                        }
                    }
                }
                "alias" => {
                    for (name, expansion) in &self.aliases {
                        if name.starts_with(prefix) {
                            out.push((name.clone(), Some(format!("alias: {expansion}"))));
                        }
                    }
                }
                "variable" => {
                    for name in self.variable_names() {
                        if name.starts_with(prefix) {
                            out.push((name, Some(String::from("var"))));
                        }
                    }
                }
                "directory" => {
                    if let Ok(entries) = std::fs::read_dir(&self.cwd) {
                        for entry in entries.flatten() {
                            if entry.path().is_dir() {
                                let name = entry.file_name().to_string_lossy().into_owned();
                                if name.starts_with(prefix) {
                                    out.push((format!("{name}/"), Some(String::from("dir"))));
                                }
                            }
                        }
                    }
                }
                "file" => {
                    if let Ok(entries) = std::fs::read_dir(&self.cwd) {
                        for entry in entries.flatten() {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if name.starts_with(prefix) {
                                let desc = if entry.path().is_dir() {
                                    Some(String::from("dir"))
                                } else {
                                    None
                                };
                                out.push((name, desc));
                            }
                        }
                    }
                }
                "command" => {
                    for name in self.command_words() {
                        if name.starts_with(prefix) {
                            let desc = crate::help::builtin_summary(&name).map(str::to_string);
                            out.push((name, desc));
                        }
                    }
                }
                _ => {}
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        out
    }

    fn complete_builtin(&mut self, args: &[String]) -> i32 {
        let (spec, print, remove, targets) = match Self::parse_completion_args(args, "complete", true) {
            Ok(parsed) => parsed,
            Err(code) => return code,
        };
        if remove {
            if targets.is_empty() {
                self.completions.clear();
            } else {
                for t in &targets {
                    self.completions.remove(t);
                }
            }
            return 0;
        }
        if print || (targets.is_empty() && spec.wordlist.is_empty() && spec.function.is_none() && spec.actions.is_empty()) {
            let mut keys: Vec<&String> = if targets.is_empty() {
                self.completions.keys().collect()
            } else {
                targets.iter().filter(|t| self.completions.contains_key(*t)).collect()
            };
            keys.sort();
            for name in keys {
                if let Some(s) = self.completions.get(name) {
                    let mut parts = vec![String::from("complete")];
                    for act in &s.actions {
                        match act.as_str() {
                            "file" => parts.push(String::from("-f")),
                            "directory" => parts.push(String::from("-d")),
                            "command" => parts.push(String::from("-c")),
                            "variable" => parts.push(String::from("-v")),
                            "builtin" => parts.push(String::from("-b")),
                            "alias" => parts.push(String::from("-a")),
                            other => parts.push(format!("-A {other}")),
                        }
                    }
                    if !s.wordlist.is_empty() {
                        parts.push(format!("-W '{}'", s.wordlist.join(" ")));
                    }
                    if let Some(func) = &s.function {
                        parts.push(format!("-F {func}"));
                    }
                    parts.push(name.clone());
                    println!("{}", parts.join(" "));
                }
            }
            return 0;
        }
        for cmd in targets {
            self.completions.insert(cmd, spec.clone());
        }
        0
    }

    fn compgen_builtin(&mut self, args: &[String]) -> i32 {
        let (spec, _, _, rest) = match Self::parse_completion_args(args, "compgen", false) {
            Ok(parsed) => parsed,
            Err(code) => return code,
        };
        let prefix = rest.first().map(String::as_str).unwrap_or("");
        let words = vec![String::from("compgen"), prefix.to_string()];
        let matches = self.generate_completion_candidates(&spec, "compgen", &words, 1, prefix, false);
        for (item, _) in &matches {
            println!("{item}");
        }
        if matches.is_empty() { 1 } else { 0 }
    }

    pub(crate) fn complete_command_candidates(
        &mut self,
        line: &str,
        cursor: usize,
    ) -> Option<Vec<(String, Option<String>)>> {
        let byte_idx = line
            .char_indices()
            .nth(cursor)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        let before_cursor = &line[..byte_idx];
        let seg_start = before_cursor
            .rfind(['|', ';', '&', '(', '{', '\n'])
            .map(|i| i + 1)
            .unwrap_or(0);
        let seg = before_cursor[seg_start..].trim_start();
        let trailing_ws = before_cursor.ends_with(char::is_whitespace);
        let mut tokens: Vec<String> = seg.split_whitespace().map(str::to_string).collect();
        if trailing_ws {
            tokens.push(String::new());
        }
        if tokens.len() <= 1 {
            return None;
        }
        let cmd_name = tokens[0].clone();
        let spec = self.completions.get(&cmd_name)?.clone();
        let cword = tokens.len() - 1;
        let prefix = tokens[cword].clone();
        Some(self.generate_completion_candidates(&spec, &cmd_name, &tokens, cword, &prefix, true))
    }
}

fn read_stdin_utf8() -> Result<String, i32> {
    let mut buf = String::new();
    if let Err(err) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf) {
        eprintln!("sharky: stdin: {err}");
        return Err(1);
    }
    Ok(buf)
}

fn parse_json_rows(raw: &str) -> Result<(Vec<crate::agent::json::Value>, bool), String> {
    use crate::agent::json::{Value, parse_line};
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok((Vec::new(), true));
    }
    if let Ok(val) = parse_line(trimmed.as_bytes()) {
        return match val {
            Value::Array(items) => Ok((items, true)),
            other => Ok((vec![other], false)),
        };
    }
    let mut rows = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let item = parse_line(line.as_bytes()).map_err(|e| e.to_string())?;
        rows.push(item);
    }
    Ok((rows, true))
}

fn json_path<'a>(value: &'a crate::agent::json::Value, path: &str) -> Option<&'a crate::agent::json::Value> {
    if let Some(direct) = value.get(path) {
        return Some(direct);
    }
    let mut cur = value;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn json_scalar_string(value: &crate::agent::json::Value) -> String {
    use crate::agent::json::Value;
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::String(s) => s.clone(),
        other => other.encode(),
    }
}

fn format_pretty_json(value: &crate::agent::json::Value, indent: usize) -> String {
    use crate::agent::json::Value;
    let pad = "  ".repeat(indent);
    let inner_pad = "  ".repeat(indent + 1);
    match value {
        Value::Array(items) if !items.is_empty() => {
            let mut out = String::from("[\n");
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&inner_pad);
                out.push_str(&format_pretty_json(item, indent + 1));
            }
            out.push('\n');
            out.push_str(&pad);
            out.push(']');
            out
        }
        Value::Object(entries) if !entries.is_empty() => {
            let mut out = String::from("{\n");
            for (idx, (k, v)) in entries.iter().enumerate() {
                if idx > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&inner_pad);
                out.push_str(&Value::String(k.clone()).encode());
                out.push_str(": ");
                out.push_str(&format_pretty_json(v, indent + 1));
            }
            out.push('\n');
            out.push_str(&pad);
            out.push('}');
            out
        }
        other => other.encode(),
    }
}

fn from_json_builtin(args: &[String]) -> i32 {
    use crate::agent::json::{Value, parse_line};
    let jsonl = args.iter().any(|a| a == "--jsonl");
    let raw = match read_stdin_utf8() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        println!("[]");
        return 0;
    }
    if jsonl {
        match parse_json_rows(trimmed) {
            Ok((rows, _)) => {
                println!("{}", Value::Array(rows).encode());
                0
            }
            Err(err) => {
                eprintln!("sharky: from-json: {err}");
                1
            }
        }
    } else if let Ok(val) = parse_line(trimmed.as_bytes()) {
        println!("{}", val.encode());
        0
    } else {
        match parse_json_rows(trimmed) {
            Ok((rows, _)) => {
                println!("{}", Value::Array(rows).encode());
                0
            }
            Err(err) => {
                eprintln!("sharky: from-json: {err}");
                1
            }
        }
    }
}

fn to_json_builtin(args: &[String]) -> i32 {
    use crate::agent::json::{Value, parse_line};
    let pretty = args.iter().any(|a| a == "--pretty");
    let raw = match read_stdin_utf8() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let trimmed = raw.trim();
    let val = if let Ok(parsed) = parse_line(trimmed.as_bytes()) {
        parsed
    } else {
        Value::Array(
            raw.lines()
                .filter(|line| !line.is_empty())
                .map(|line| Value::String(line.to_string()))
                .collect(),
        )
    };
    if pretty {
        println!("{}", format_pretty_json(&val, 0));
    } else {
        println!("{}", val.encode());
    }
    0
}

fn where_builtin(args: &[String]) -> i32 {
    use crate::agent::json::Value;
    if args.len() < 3 {
        eprintln!("sharky: where: usage: where FIELD OP VALUE");
        return 2;
    }
    let field = &args[0];
    let op = args[1].as_str();
    let rhs = &args[2];
    let raw = match read_stdin_utf8() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let (rows, _) = match parse_json_rows(&raw) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("sharky: where: {err}");
            return 1;
        }
    };
    let re = if op == "=~" {
        match regex_lite::Regex::new(rhs) {
            Ok(re) => Some(re),
            Err(err) => {
                eprintln!("sharky: where: invalid regex: {err}");
                return 2;
            }
        }
    } else {
        None
    };

    let mut kept = Vec::new();
    for row in rows {
        let Some(lhs_val) = json_path(&row, field) else {
            continue;
        };
        let lhs_str = json_scalar_string(lhs_val);
        let matched = match op {
            "==" | "eq" => {
                if let (Some(a), Ok(b)) = (lhs_val.as_i64(), rhs.parse::<i64>()) {
                    a == b
                } else {
                    lhs_str == *rhs
                }
            }
            "!=" | "ne" => {
                if let (Some(a), Ok(b)) = (lhs_val.as_i64(), rhs.parse::<i64>()) {
                    a != b
                } else {
                    lhs_str != *rhs
                }
            }
            ">" | "gt" | ">=" | "ge" | "<" | "lt" | "<=" | "le" => {
                let cmp = if let (Some(a), Ok(b)) = (lhs_val.as_i64(), rhs.parse::<i64>()) {
                    a.cmp(&b)
                } else {
                    lhs_str.cmp(rhs)
                };
                match op {
                    ">" | "gt" => cmp.is_gt(),
                    ">=" | "ge" => cmp.is_ge(),
                    "<" | "lt" => cmp.is_lt(),
                    _ => cmp.is_le(),
                }
            }
            "contains" => {
                if let Some(items) = lhs_val.as_array() {
                    items.iter().any(|v| json_scalar_string(v) == *rhs)
                } else {
                    lhs_str.contains(rhs.as_str())
                }
            }
            "=~" => re.as_ref().is_some_and(|r| r.is_match(&lhs_str)),
            other => {
                eprintln!("sharky: where: unknown operator '{other}'");
                return 2;
            }
        };
        if matched {
            kept.push(row);
        }
    }
    println!("{}", Value::Array(kept).encode());
    0
}

fn select_builtin(args: &[String]) -> i32 {
    use crate::agent::json::Value;
    if args.is_empty() {
        eprintln!("sharky: select: usage: select FIELD ...");
        return 2;
    }
    let raw = match read_stdin_utf8() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let (rows, was_array) = match parse_json_rows(&raw) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("sharky: select: {err}");
            return 1;
        }
    };
    let projected: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            let mut entries = Vec::with_capacity(args.len());
            for field in args {
                let val = json_path(&row, field).cloned().unwrap_or(Value::Null);
                entries.push((field.clone(), val));
            }
            Value::Object(entries)
        })
        .collect();
    if !was_array && projected.len() == 1 {
        println!("{}", projected[0].encode());
    } else {
        println!("{}", Value::Array(projected).encode());
    }
    0
}

fn table_builtin(_args: &[String]) -> i32 {
    use crate::agent::json::Value;
    let raw = match read_stdin_utf8() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let (rows, _) = match parse_json_rows(&raw) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("sharky: table: {err}");
            return 1;
        }
    };
    if rows.is_empty() {
        println!("(empty)");
        return 0;
    }

    let mut columns: Vec<String> = Vec::new();
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    if rows.iter().all(|v| v.as_object().is_some()) {
        for row in &rows {
            if let Some(entries) = row.as_object() {
                for (k, _) in entries {
                    if !columns.contains(k) {
                        columns.push(k.clone());
                    }
                }
            }
        }
        for row in &rows {
            let cells = columns
                .iter()
                .map(|col| row.get(col).map(json_scalar_string).unwrap_or_default())
                .collect();
            table_rows.push(cells);
        }
    } else {
        columns.push(String::from("value"));
        for row in &rows {
            table_rows.push(vec![json_scalar_string(row)]);
        }
    }

    if columns.is_empty() {
        println!("(empty)");
        return 0;
    }

    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for row in &table_rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let border = |left: char, mid: char, right: char| -> String {
        let mut s = String::new();
        s.push(left);
        for (i, &w) in widths.iter().enumerate() {
            if i > 0 {
                s.push(mid);
            }
            for _ in 0..(w + 2) {
                s.push('─');
            }
        }
        s.push(right);
        s
    };
    let render_row = |cells: &[String]| -> String {
        let mut s = String::from("│");
        for (i, cell) in cells.iter().enumerate() {
            let len = cell.chars().count();
            let pad = widths[i].saturating_sub(len);
            s.push(' ');
            s.push_str(cell);
            for _ in 0..pad {
                s.push(' ');
            }
            s.push_str(" │");
        }
        s
    };

    println!("{}", border('┌', '┬', '┐'));
    println!("{}", render_row(&columns));
    println!("{}", border('├', '┼', '┤'));
    for row in &table_rows {
        println!("{}", render_row(row));
    }
    println!("{}", border('└', '┴', '┘'));
    let _ = Value::Null;
    0
}

fn is_builtin_name(name: &str) -> bool {
    matches!(
        name,
        ":" | "true"
            | "false"
            | "cd"
            | "pushd"
            | "popd"
            | "dirs"
            | "ulimit"
            | "hash"
            | "let"
            | "times"
            | "exit"
            | "export"
            | "unset"
            | "set"
            | "source"
            | "."
            | "break"
            | "continue"
            | "return"
            | "jobs"
            | "fg"
            | "bg"
            | "disown"
            | "pwd"
            | "type"
            | "alias"
            | "abbr"
            | "unabbr"
            | "unalias"
            | "trap"
            | "read"
            | "mapfile"
            | "readarray"
            | "history"
            | "wake"
            | "wait"
            | "kill"
            | "z"
            | "zi"
            | "reload"
            | "shift"
            | "declare"
            | "local"
            | "readonly"
            | "typeset"
            | "shopt"
            | "command"
            | "builtin"
            | "caller"
            | "complete"
            | "compgen"
            | "prevd"
            | "nextd"
            | "from-json"
            | "to-json"
            | "where"
            | "select"
            | "table"
            | "exec"
            | "time"
            | "help"
            | "echo"
            | "printf"
            | "test"
            | "["
            | "eval"
            | "umask"
            | "getopts"
    )
}

fn is_declaration_word(name: &str) -> bool {
    matches!(name, "declare" | "local" | "readonly" | "typeset")
}

impl test_expr::Context for Shell {
    fn expand(&mut self, word: &Word) -> String {
        self.expand_text_word(word)
    }

    fn var_is_set(&self, expr: &str) -> bool {
        if let Some((name, sub)) = expr.strip_suffix(']').and_then(|s| s.split_once('['))
            && !name.is_empty()
        {
            let Ok((target, _)) = self.resolve_nameref_ref(name) else {
                return false;
            };
            let target = target.as_ref();
            if let Some(assoc) = self.assoc.get(target) {
                return if sub == "@" || sub == "*" {
                    !assoc.is_empty()
                } else {
                    assoc.contains_key(sub)
                };
            }
            if let Some(array) = self.arrays.get(target) {
                if sub == "@" || sub == "*" {
                    return array.iter().any(Option::is_some);
                }
                let Ok(idx) = sub.trim().parse::<isize>() else {
                    return false;
                };
                let resolved = if idx < 0 {
                    isize::try_from(array.len())
                        .ok()
                        .and_then(|len| len.checked_add(idx))
                        .unwrap_or(-1)
                } else {
                    idx
                };
                return usize::try_from(resolved)
                    .ok()
                    .and_then(|i| array.get(i))
                    .is_some_and(Option::is_some);
            }
            return matches!(sub, "0" | "@" | "*") && self.scalar_is_set(target);
        }

        let Ok((target, sub)) = self.resolve_nameref_ref(expr) else {
            return false;
        };
        let target = target.as_ref();
        if self.namerefs.get(target).is_some_and(Option::is_none) {
            return false;
        }
        if let Some(sub) = sub {
            if let Some(assoc) = self.assoc.get(target) {
                return assoc.contains_key(&sub);
            }
            if let Some(array) = self.arrays.get(target) {
                return sub
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| array.get(i))
                    .is_some_and(Option::is_some);
            }
            return sub.trim() == "0" && self.scalar_is_set(target);
        }
        if let Some(assoc) = self.assoc.get(target) {
            return assoc.contains_key("0");
        }
        if let Some(array) = self.arrays.get(target) {
            return array.first().is_some_and(Option::is_some);
        }
        self.scalar_is_set(target)
    }

    fn option_is_set(&self, name: &str) -> bool {
        match name {
            "errexit" => self.errexit,
            "nounset" => self.nounset,
            "xtrace" => self.xtrace,
            "histexpand" => self.histexpand,
            "pipefail" => self.pipefail,
            "noclobber" => self.noclobber,
            "vi" => self.vi,
            _ => false,
        }
    }

    fn set_rematch(&mut self, groups: Vec<String>) {
        if groups.is_empty() {
            self.arrays.remove("BASH_REMATCH");
        } else {
            self.arrays.insert(
                "BASH_REMATCH".to_string(),
                groups.into_iter().map(Some).collect(),
            );
        }
    }

    fn nocasematch(&self) -> bool {
        self.nocasematch
    }

    fn extglob(&self) -> bool {
        self.extglob
    }
}

fn initial_random_seed() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(1);
    std::process::id() ^ nanos ^ 0x9e37_79b9
}

fn normalize_logical_path(base: &Path, target: &Path) -> PathBuf {
    use std::path::Component;
    let combined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    };
    let mut out = PathBuf::new();
    for comp in combined.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

fn is_special_name(name: &str) -> bool {
    matches!(
        name,
        "?" | "#"
            | "@"
            | "*"
            | "!"
            | "$"
            | "-"
            | "RANDOM"
            | "SECONDS"
            | "BASHPID"
            | "UID"
            | "EUID"
            | "LINENO"
    ) || (!name.is_empty() && name.chars().all(|char| char.is_ascii_digit()))
}

fn is_valid_array_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}

// Bash's hash_string (FNV-1, multiply then xor) masked to the 1024 buckets an
// associative array is created with. Iterating buckets reproduces the key
// order of bash 5.3's `declare -p`; keys that land in the same bucket keep
// this shell's own order because insertion order is not stored.
fn bash_assoc_bucket(key: &str) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for byte in key.bytes() {
        hash = hash.wrapping_mul(16_777_619) ^ u32::from(byte);
    }
    hash & 1023
}

/// Stores one scalar, replacing an existing entry in place so the order of the
/// environment is preserved.
fn set_env_entry(env: &mut Vec<(String, String)>, name: &str, value: String) {
    match env.iter_mut().find(|(key, _)| key == name) {
        Some(entry) => entry.1 = value,
        None => env.push((name.to_string(), value)),
    }
}

/// Bash's `atoi` for the `SHLVL` adjustment path: leading space, an optional
/// sign, then digits, stopping at the first non-digit. Any overflow saturates.
fn bash_atoi(text: &str) -> i64 {
    let mut chars = text.chars().peekable();
    while chars.peek().is_some_and(|char| char.is_ascii_whitespace()) {
        chars.next();
    }
    let mut negative = false;
    match chars.peek() {
        Some('-') => {
            negative = true;
            chars.next();
        }
        Some('+') => {
            chars.next();
        }
        _ => {}
    }
    let mut value: i64 = 0;
    let mut digits = false;
    while let Some(char) = chars.peek() {
        let Some(digit) = char.to_digit(10) else {
            break;
        };
        digits = true;
        value = value.saturating_mul(10).saturating_add(digit as i64);
        chars.next();
    }
    if !digits {
        return 0;
    }
    if negative { value.saturating_neg() } else { value }
}

/// The simple command bash would exec without forking at the end of an input
/// unit. The path only follows shapes bash optimizes: the last list entry, its
/// last `&&`/`||` element, a single-command pipeline, plain redirection-free
/// simple commands and nested subshells. Groups, functions, `if`, loops,
/// negation, pipelines with more than one member and redirected commands all
/// fork in bash and are deliberately excluded.
fn final_simple(program: &Program) -> Option<*const Simple> {
    final_simple_in_list(program.lines.last()?)
}

fn final_simple_in_list(list: &List) -> Option<*const Simple> {
    let entry = list.entries.last()?;
    if entry.background {
        return None;
    }
    let pipeline = match entry.and_or.rest.last() {
        Some((_, pipeline)) => pipeline,
        None => &entry.and_or.first,
    };
    final_simple_in_pipeline(pipeline)
}

fn final_simple_in_pipeline(pipeline: &Pipeline) -> Option<*const Simple> {
    if pipeline.timed.is_some() || pipeline.negate || pipeline.commands.len() != 1 {
        return None;
    }
    match &pipeline.commands[0] {
        Command::Simple(simple) => Some(simple as *const Simple),
        Command::Subshell(list) => final_simple_in_list(list),
        _ => None,
    }
}

fn quote_value(value: &str) -> String {
    if value.chars().any(char::is_control) {
        ansi_quote(value)
    } else {
        double_quote(value)
    }
}

fn bash_single_quote(value: &str) -> String {
    if value.chars().any(char::is_control) {
        ansi_quote(value)
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn unescape_ansi_c(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('a') => out.push('\x07'),
            Some('b') => out.push('\x08'),
            Some('e' | 'E') => out.push('\x1b'),
            Some('f') => out.push('\x0c'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('v') => out.push('\x0b'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn ansi_quote(value: &str) -> String {
    let mut quoted = String::from("$'");
    for char in value.chars() {
        match char {
            '\x1b' => quoted.push_str("\\E"),
            '\x07' => quoted.push_str("\\a"),
            '\x0b' => quoted.push_str("\\v"),
            '\x08' => quoted.push_str("\\b"),
            '\x0c' => quoted.push_str("\\f"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("\\'"),
            char if char.is_control() => {
                for byte in char.to_string().bytes() {
                    quoted.push_str(&format!("\\{byte:03o}"));
                }
            }
            char => quoted.push(char),
        }
    }
    quoted.push('\'');
    quoted
}

fn double_quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    for char in value.chars() {
        if matches!(char, '\\' | '`' | '$' | '"') {
            quoted.push('\\');
        }
        quoted.push(char);
    }
    quoted.push('"');
    quoted
}

fn quote_assoc_key(key: &str) -> String {
    if key.chars().any(char::is_control) {
        ansi_quote(key)
    } else if key == "@" || key == "*" || contains_shell_metas(key) {
        double_quote(key)
    } else {
        key.to_string()
    }
}

fn contains_shell_metas(value: &str) -> bool {
    let mut previous = None;
    for char in value.chars() {
        if matches!(
            char,
            ' ' | '\t'
                | '\n'
                | '\''
                | '"'
                | '\\'
                | '|'
                | '&'
                | ';'
                | '('
                | ')'
                | '<'
                | '>'
                | '!'
                | '{'
                | '}'
                | '*'
                | '['
                | '?'
                | ']'
                | '^'
                | '$'
                | '`'
        ) {
            return true;
        }
        if char == '~' && (previous.is_none() || matches!(previous, Some('=') | Some(':'))) {
            return true;
        }
        if char == '#' && previous.is_none() {
            return true;
        }
        previous = Some(char);
    }
    false
}

/// How `trap` spells an installed signal: `EXIT` stays bare, everything else
/// carries the `SIG` prefix bash prints.
fn trap_label(signal: &str) -> String {
    if signal == "EXIT" {
        signal.to_string()
    } else {
        format!("SIG{signal}")
    }
}

/// Canonical trap name for a specification: `EXIT` or `0`, a signal name with
/// an optional `SIG` prefix in any case, or a signal number.
fn trap_signal(name: &str) -> Option<&'static str> {
    let upper = name.to_ascii_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    if bare == "EXIT" || bare == "0" {
        return Some("EXIT");
    }
    if bare == "ERR" {
        return Some("ERR");
    }
    if bare == "RETURN" {
        return Some("RETURN");
    }
    if let Ok(number) = bare.parse::<i32>() {
        return crate::signal::name(number);
    }
    crate::signal::number(bare).and_then(crate::signal::name)
}

/// One line of standard input, or the reason there is none.
enum LineInput {
    Line(String),
    End,
    /// A signal asked the shell to stop reading; the caller reports 130 and
    /// lets the run loop unwind the command being executed.
    Interrupted,
}

fn read_stdin_line() -> LineInput {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        // SAFETY: fd 0 is the shell's stdin and the buffer is one byte.
        let count = unsafe { nix::libc::read(0, byte.as_mut_ptr() as *mut _, 1) };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                // Without SA_RESTART the handler interrupts the read, which is
                // the only way a blocked `read` builtin learns about Ctrl-C.
                if crate::signal::interrupt_pending() || crate::signal::fatal_signal().is_some() {
                    return LineInput::Interrupted;
                }
                continue;
            }
            break;
        }
        if count == 0 {
            break;
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }

    if bytes.is_empty() {
        LineInput::End
    } else {
        LineInput::Line(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Options accepted by the `read` builtin, parsed before any record is read.
struct ReadOptions {
    raw: bool,
    /// `-n`: stop at the delimiter or after this many characters.
    chars: Option<usize>,
    /// `-N`: take exactly this many characters, delimiter included.
    exact_chars: Option<usize>,
    /// `-a`: split the record into this indexed array.
    array: Option<String>,
    /// `-t`: give up after this long.
    timeout: Option<std::time::Duration>,
    /// `-d`: byte that ends the record; an empty argument means NUL.
    delimiter: u8,
    /// `-p`: prompt shown only when standard input is a terminal.
    prompt: Option<String>,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            raw: false,
            chars: None,
            exact_chars: None,
            array: None,
            timeout: None,
            delimiter: b'\n',
            prompt: None,
        }
    }
}

/// Parse leading options for `read`, returning the options and the index of the
/// first name. A value may be attached (`-n1`) or the next argument (`-n 1`);
/// flags combine (`-rn1`). Bash stops option parsing at the first non-option.
fn parse_read_options(args: &[String]) -> Result<(ReadOptions, usize), i32> {
    let mut options = ReadOptions::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            index += 1;
            break;
        }
        let Some(body) = arg.strip_prefix('-') else {
            break;
        };
        if body.is_empty() {
            break;
        }
        let mut offset = 0;
        let mut consumed_next = false;
        while offset < body.len() {
            let flag = body[offset..].chars().next().expect("offset is a boundary");
            offset += flag.len_utf8();
            match flag {
                'r' => options.raw = true,
                'n' | 'N' | 'a' | 't' | 'd' | 'p' => {
                    let attached = &body[offset..];
                    let value = if !attached.is_empty() {
                        attached.to_string()
                    } else if let Some(next) = args.get(index + 1) {
                        consumed_next = true;
                        next.clone()
                    } else {
                        read_option_usage(flag, true);
                        return Err(2);
                    };
                    offset = body.len();
                    match flag {
                        'n' => options.chars = Some(read_count(&value)?),
                        'N' => options.exact_chars = Some(read_count(&value)?),
                        'a' => {
                            if !is_valid_array_name(&value) {
                                eprintln!("sharky: read: {value}: not a valid identifier");
                                return Err(1);
                            }
                            options.array = Some(value);
                        }
                        't' => {
                            let seconds: f64 = value.parse().map_err(|_| {
                                eprintln!("sharky: read: {value}: invalid timeout");
                                1
                            })?;
                            // `Duration::from_secs_f64` panics for values it
                            // cannot represent. `u64::MAX as f64` rounds up to
                            // 2^64, the first value that overflows the Duration
                            // seconds field, so reject it and everything above.
                            if !seconds.is_finite()
                                || seconds < 0.0
                                || seconds >= u64::MAX as f64
                            {
                                eprintln!("sharky: read: {value}: invalid timeout");
                                return Err(1);
                            }
                            options.timeout =
                                Some(std::time::Duration::from_secs_f64(seconds));
                        }
                        'd' => {
                            options.delimiter =
                                value.as_bytes().first().copied().unwrap_or(0);
                        }
                        'p' => options.prompt = Some(value),
                        _ => unreachable!("matched above"),
                    }
                }
                _ => {
                    read_option_usage(flag, false);
                    return Err(2);
                }
            }
        }
        index += if consumed_next { 2 } else { 1 };
    }
    Ok((options, index))
}

fn read_count(value: &str) -> Result<usize, i32> {
    value.parse().map_err(|_| {
        eprintln!("sharky: read: {value}: invalid number");
        1
    })
}

fn read_option_usage(flag: char, missing_argument: bool) {
    if missing_argument {
        eprintln!("sharky: read: -{flag}: option requires an argument");
    } else {
        eprintln!("sharky: read: -{flag}: invalid option");
    }
    eprintln!(
        "sharky: read: usage: read [-r] [-a array] [-d delim] [-n nchars] [-N nchars] [-p prompt] [-t timeout] [name ...]"
    );
}

/// One record of standard input, with the delimiter consumed but not included.
enum RecordInput {
    Record { text: String, terminated: bool },
    /// The timeout expired; the partial record is still handed to the caller.
    Timeout(String),
    Interrupted,
}

/// The outcome of reading a fixed number of characters with `-n`/`-N`.
enum CharsOutcome {
    /// The character limit or the delimiter was reached.
    Done,
    /// End of input arrived first.
    End,
    Timeout,
    Interrupted,
}

/// One byte (or the reason there is none) from standard input. With a timeout,
/// readiness is awaited with `poll` so the builtin never busy-waits.
enum ByteInput {
    Byte(u8),
    End,
    Timeout,
    Interrupted,
}

/// Reads standard input one byte at a time under an optional total deadline.
struct StdinReader {
    deadline: Option<std::time::Instant>,
}

impl StdinReader {
    fn new(timeout: Option<std::time::Duration>) -> Self {
        Self {
            deadline: timeout.map(|timeout| std::time::Instant::now() + timeout),
        }
    }

    fn next(&mut self) -> ByteInput {
        if let Some(deadline) = self.deadline {
            loop {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return ByteInput::Timeout;
                }
                let remaining = deadline.saturating_duration_since(now);
                let millis = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
                let mut descriptor = nix::libc::pollfd {
                    fd: 0,
                    events: nix::libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid descriptor and the buffer it describes.
                let result = unsafe { nix::libc::poll(&mut descriptor, 1, millis) };
                if result > 0 {
                    break;
                }
                if result < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        if crate::signal::interrupt_pending()
                            || crate::signal::fatal_signal().is_some()
                        {
                            return ByteInput::Interrupted;
                        }
                        continue;
                    }
                    return ByteInput::End;
                }
                // `poll` can return early after a signal; the loop rechecks the
                // clock before waiting again.
            }
        }

        loop {
            let mut byte = [0u8; 1];
            // SAFETY: fd 0 is the shell's stdin and the buffer is one byte.
            let count = unsafe { nix::libc::read(0, byte.as_mut_ptr() as *mut _, 1) };
            if count < 0 {
                if std::io::Error::last_os_error().kind()
                    == std::io::ErrorKind::Interrupted
                {
                    // Without SA_RESTART the handler interrupts the read, which
                    // is the only way a blocked `read` learns about Ctrl-C.
                    if crate::signal::interrupt_pending()
                        || crate::signal::fatal_signal().is_some()
                    {
                        return ByteInput::Interrupted;
                    }
                    continue;
                }
                return ByteInput::End;
            }
            if count == 0 {
                return ByteInput::End;
            }
            return ByteInput::Byte(byte[0]);
        }
    }
}

fn read_stdin_record(delimiter: u8, timeout: Option<std::time::Duration>) -> RecordInput {
    let mut reader = StdinReader::new(timeout);
    let mut bytes = Vec::new();
    loop {
        match reader.next() {
            ByteInput::Byte(byte) => {
                if byte == delimiter {
                    return RecordInput::Record {
                        text: String::from_utf8_lossy(&bytes).into_owned(),
                        terminated: true,
                    };
                }
                bytes.push(byte);
            }
            ByteInput::End => {
                return RecordInput::Record {
                    text: String::from_utf8_lossy(&bytes).into_owned(),
                    terminated: false,
                };
            }
            ByteInput::Timeout => {
                return RecordInput::Timeout(String::from_utf8_lossy(&bytes).into_owned());
            }
            ByteInput::Interrupted => return RecordInput::Interrupted,
        }
    }
}

fn read_stdin_chars(
    limit: usize,
    delimiter: Option<u8>,
    timeout: Option<std::time::Duration>,
) -> (String, CharsOutcome) {
    let mut reader = StdinReader::new(timeout);
    let mut bytes = Vec::new();
    loop {
        match reader.next() {
            ByteInput::Byte(byte) => {
                if delimiter == Some(byte) {
                    return (
                        String::from_utf8_lossy(&bytes).into_owned(),
                        CharsOutcome::Done,
                    );
                }
                bytes.push(byte);
                if complete_char_count(&bytes).is_some_and(|count| count >= limit) {
                    return (
                        String::from_utf8_lossy(&bytes).into_owned(),
                        CharsOutcome::Done,
                    );
                }
            }
            ByteInput::End => {
                return (
                    String::from_utf8_lossy(&bytes).into_owned(),
                    CharsOutcome::End,
                );
            }
            ByteInput::Timeout => {
                return (
                    String::from_utf8_lossy(&bytes).into_owned(),
                    CharsOutcome::Timeout,
                );
            }
            ByteInput::Interrupted => {
                return (
                    String::from_utf8_lossy(&bytes).into_owned(),
                    CharsOutcome::Interrupted,
                );
            }
        }
    }
}

/// Number of complete Unicode scalar values in `bytes`, or `None` while a
/// multibyte character is still waiting for its remaining bytes.
fn complete_char_count(bytes: &[u8]) -> Option<usize> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text.chars().count()),
        Err(error) if error.error_len().is_none() => None,
        Err(_) => Some(String::from_utf8_lossy(bytes).chars().count()),
    }
}

/// Split every field of a record for `read -a`, with bash's word-splitting
/// rules: leading IFS whitespace is skipped and a delimiter that follows a
/// field is consumed with it, so a final delimiter adds no field of its own.
fn split_read_words(line: &str, ifs: &str, escapes: bool) -> Vec<String> {
    let mut chars: Vec<(char, bool)> = Vec::with_capacity(line.len());
    let mut source = line.chars();
    while let Some(char) = source.next() {
        if escapes && char == '\\' {
            if let Some(next) = source.next() {
                chars.push((next, true));
            }
            continue;
        }
        chars.push((char, false));
    }

    let ifs: Vec<char> = ifs.chars().collect();
    let is_ifs = |char: char| ifs.contains(&char);
    let is_ifs_space = |char: char| is_ifs(char) && matches!(char, ' ' | '\t' | '\n');

    let mut fields = Vec::new();
    let mut position = 0;
    while position < chars.len() {
        let (char, escaped) = chars[position];
        if !escaped && is_ifs_space(char) {
            position += 1;
            continue;
        }
        let (word, next) = read_word(&chars, position, &is_ifs, &is_ifs_space);
        fields.push(word);
        if next <= position {
            break;
        }
        position = next;
    }
    fields
}

fn dequote_read_line(line: &str, escapes: bool) -> String {
    if !escapes {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(char) = chars.next() {
        if char == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(char);
        }
    }
    out
}

/// Split a line for `read` as bash does: leading IFS whitespace is skipped,
/// every name but the last takes a field, and the last name receives the rest of
/// the line with its delimiters. A trailing IFS character ends the remainder.
fn split_read_fields(line: &str, ifs: &str, escapes: bool, count: usize) -> Vec<String> {
    // Mark escaped characters so they never act as delimiters.
    let mut chars: Vec<(char, bool)> = Vec::with_capacity(line.len());
    let mut source = line.chars();
    while let Some(char) = source.next() {
        if escapes && char == '\\' {
            if let Some(next) = source.next() {
                chars.push((next, true));
            }
            continue;
        }
        chars.push((char, false));
    }

    let ifs: Vec<char> = ifs.chars().collect();
    let is_ifs = |char: char| ifs.contains(&char);
    let is_ifs_space = |char: char| is_ifs(char) && matches!(char, ' ' | '\t' | '\n');

    let mut start = 0;
    while start < chars.len() && !chars[start].1 && is_ifs_space(chars[start].0) {
        start += 1;
    }

    let mut fields = Vec::new();
    let mut position = start;
    for _ in 0..count.saturating_sub(1) {
        if position >= chars.len() {
            fields.push(String::new());
            continue;
        }
        let (word, next) = read_word(&chars, position, &is_ifs, &is_ifs_space);
        fields.push(word);
        position = next;
    }

    let last = if position >= chars.len() {
        String::new()
    } else {
        let (word, next) = read_word(&chars, position, &is_ifs, &is_ifs_space);
        if next >= chars.len() {
            word
        } else {
            strip_trailing_ifs_space(&chars[position..], &is_ifs_space)
        }
    };
    fields.push(last);
    fields
}

/// Take one IFS-delimited word and advance past its delimiter. A delimiter is a
/// run of IFS whitespace, at most one non-whitespace IFS character, and the IFS
/// whitespace after it.
fn read_word(
    chars: &[(char, bool)],
    start: usize,
    is_ifs: &impl Fn(char) -> bool,
    is_ifs_space: &impl Fn(char) -> bool,
) -> (String, usize) {
    let mut end = start;
    while end < chars.len() && (chars[end].1 || !is_ifs(chars[end].0)) {
        end += 1;
    }
    let word = chars[start..end].iter().map(|(char, _)| *char).collect();

    let mut next = end;
    if next >= chars.len() || chars[next].1 {
        return (word, next);
    }
    let delimiter = chars[next].0;
    if is_ifs_space(delimiter) {
        while next < chars.len() && !chars[next].1 && is_ifs_space(chars[next].0) {
            next += 1;
        }
        if next >= chars.len() || chars[next].1 || !is_ifs(chars[next].0) {
            return (word, next);
        }
    } else if !is_ifs(delimiter) {
        return (word, next);
    }
    next += 1;
    while next < chars.len() && !chars[next].1 && is_ifs_space(chars[next].0) {
        next += 1;
    }
    (word, next)
}

fn strip_trailing_ifs_space(chars: &[(char, bool)], is_ifs_space: &impl Fn(char) -> bool) -> String {
    let mut end = chars.len();
    while end > 0 {
        let (char, escaped) = chars[end - 1];
        if escaped || !is_ifs_space(char) {
            break;
        }
        end -= 1;
    }
    chars[..end].iter().map(|(char, _)| *char).collect()
}

/// Returns the target word when `source` is a single input redirection and
/// nothing else. That is the only command substitution body bash reads as a
/// file instead of executing; words, assignments, extra redirects, pipelines
/// and compound commands all keep the normal subshell path.
fn redirect_only_input_target(source: &str) -> Option<Word> {
    let program = parser::parse(source).ok()?;
    let [list] = program.lines.as_slice() else {
        return None;
    };
    let [entry] = list.entries.as_slice() else {
        return None;
    };
    if entry.background || !entry.and_or.rest.is_empty() {
        return None;
    }
    let pipeline = &entry.and_or.first;
    if pipeline.negate || pipeline.timed.is_some() || pipeline.commands.len() != 1 {
        return None;
    }
    let Command::Simple(simple) = &pipeline.commands[0] else {
        return None;
    };
    if !simple.words.is_empty() || !simple.assignments.is_empty() {
        return None;
    }
    let [redirect] = simple.redirects.as_slice() else {
        return None;
    };
    if redirect.fd != 0 || !matches!(redirect.op, RedirectOp::In) {
        return None;
    }
    redirect.target.clone()
}

/// Bash drops every NUL byte from a command substitution result and warns once
/// per substitution, because a NUL cannot survive in a C string either.
fn discard_null_bytes(mut bytes: Vec<u8>) -> Vec<u8> {
    if !bytes.contains(&0) {
        return bytes;
    }
    eprintln!("sharky: warning: command substitution: ignored null byte in input");
    bytes.retain(|byte| *byte != 0);
    bytes
}

/// `len` CSPRNG bytes from the system random device, rendered as lowercase hex.
///
/// Temporary names carry a random suffix so a local user cannot predict the
/// next path and pre-create it (a denial of service) or plant a symlink at it.
/// Returns `None` only when the random device is unavailable; callers then fail
/// closed rather than falling back to a guessable name.
fn random_hex(len: usize) -> Option<String> {
    use std::io::Read;

    let mut urandom = std::fs::File::open("/dev/urandom").ok()?;
    let mut bytes = vec![0u8; len];
    urandom.read_exact(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn temp_file(text: &str) -> Result<std::fs::File, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // The descriptor is all a here-document needs, so the name is removed as
    // soon as it exists: from then on there is no path another process could
    // open, replace or follow. `create_new` refuses a path that already exists,
    // a symlink included, so a planted link can only cost a retry and never
    // redirect the write. The suffix comes from the CSPRNG so the name cannot
    // be predicted and occupied ahead of time.
    let dirs = crate::agent::capture::temp_dirs();

    let mut created = None;
    let mut failure = String::new();
    'dirs: for dir in &dirs {
        for _ in 0..4 {
            let suffix =
                random_hex(16).ok_or_else(|| "heredoc: cannot read /dev/urandom".to_string())?;
            let name = format!("sharky-here-{}-{suffix}", std::process::id());
            let path = dir.join(name);
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    let _ = std::fs::remove_file(&path);
                    created = Some(file);
                    break 'dirs;
                }
                Err(err) => failure = format!("heredoc: {err}"),
            }
        }
    }

    let mut file = created.ok_or(failure)?;
    file.write_all(text.as_bytes())
        .map_err(|err| format!("heredoc: {err}"))?;
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|err| format!("heredoc: {err}"))?;
    Ok(file)
}

fn move_fd(from: RawFd, to: RawFd) -> Result<(), String> {
    if unsafe { nix::libc::dup2(from, to) } < 0 {
        return Err(format!("redirect: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

struct SavedFd {
    target: RawFd,
    copy: Option<RawFd>,
    flags: i32,
}

// Saves every descriptor the actions touch above every descriptor they
// reference, so a nested save cannot clobber the enclosing one.
fn save_fds(actions: &[Action]) -> Result<Vec<SavedFd>, String> {
    let mut targets = Vec::new();
    let mut floor = 10;

    for action in actions {
        let (target, from) = action.fds();
        floor = floor.max(target.saturating_add(1));
        if let Some(from) = from {
            floor = floor.max(from.saturating_add(1));
        }
        if !targets.contains(&target) {
            targets.push(target);
        }
    }

    let mut saved = Vec::with_capacity(targets.len());
    for target in targets {
        // SAFETY: reads this process's own descriptor table.
        let flags = unsafe { nix::libc::fcntl(target, nix::libc::F_GETFD) };
        if flags < 0 {
            // F_GETFD only fails here for a descriptor that is already closed.
            saved.push(SavedFd {
                target,
                copy: None,
                flags: 0,
            });
            continue;
        }

        // SAFETY: the copy is a new descriptor owned by this process.
        let copy = unsafe { nix::libc::fcntl(target, nix::libc::F_DUPFD_CLOEXEC, floor) };
        if copy < 0 {
            let message = format!("redirect: {}", std::io::Error::last_os_error());
            restore_fds(saved);
            return Err(message);
        }
        saved.push(SavedFd {
            target,
            copy: Some(copy),
            flags,
        });
    }

    Ok(saved)
}

fn restore_fds(saved: Vec<SavedFd>) {
    for entry in saved {
        match entry.copy {
            Some(copy) => {
                // SAFETY: both descriptors are owned by this process.
                unsafe {
                    nix::libc::dup2(copy, entry.target);
                    nix::libc::fcntl(entry.target, nix::libc::F_SETFD, entry.flags);
                    nix::libc::close(copy);
                }
            }
            // SAFETY: the descriptor was closed before the command ran.
            None => {
                unsafe { nix::libc::close(entry.target) };
            }
        }
    }
}

// Gives `file` to `fd` without closing the target when the file already landed
// on it, and keeps the target inheritable the way a dup2 does.
fn apply_file(file: std::fs::File, fd: RawFd) -> Result<(), String> {
    let raw = file.into_raw_fd();
    if let Err(message) = move_fd(raw, fd) {
        // SAFETY: the file handed its descriptor over to us.
        unsafe { nix::libc::close(raw) };
        return Err(message);
    }

    if raw == fd {
        // SAFETY: the descriptor belongs to this process.
        unsafe { nix::libc::fcntl(fd, nix::libc::F_SETFD, 0) };
    } else {
        // SAFETY: the duplicate is no longer needed.
        unsafe { nix::libc::close(raw) };
    }
    Ok(())
}

// Opens a `>` target for truncation. With noclobber the exclusive create
// decides the race: a path that already exists may only be written when it
// resolves to something other than a regular file, which is what bash allows
// for devices and FIFOs. A missing target behind a dangling symlink is refused
// too, as bash does.
fn overwrite(path: &Path, noclobber: bool) -> std::io::Result<std::fs::File> {
    if !noclobber {
        return std::fs::File::create(path);
    }

    let exclusive = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    match exclusive {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::metadata(path) {
                Ok(metadata) if !metadata.is_file() => std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "cannot overwrite existing file",
                )),
            }
        }
        Err(err) => Err(err),
    }
}

// shortcut: allocates in the forked child; safe while sharky is single-threaded.
fn apply_actions(actions: &[Action]) -> Result<(), String> {
    for action in actions {
        match action {
            Action::Write(fd, path, noclobber) => {
                let file = overwrite(path, *noclobber)
                    .map_err(|err| format!("{}: {err}", path.display()))?;
                apply_file(file, *fd)?;
            }
            Action::Append(fd, path) => {
                let file = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path)
                    .map_err(|err| format!("{}: {err}", path.display()))?;
                apply_file(file, *fd)?;
            }
            Action::Read(fd, path) => {
                let file =
                    std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
                apply_file(file, *fd)?;
            }
            Action::Dup { from, to } => {
                move_fd(*from, *to)?;
            }
            Action::Close(fd) => {
                // Closing an already-closed descriptor is not an error.
                unsafe { nix::libc::close(*fd) };
            }
            Action::Here { fd, file } => {
                let raw = file.as_raw_fd();
                move_fd(raw, *fd)?;
                if raw == *fd {
                    // SAFETY: the temp file stays owned by the action; keep the
                    // target it became inheritable.
                    unsafe { nix::libc::fcntl(*fd, nix::libc::F_SETFD, 0) };
                }
            }
        }
    }

    Ok(())
}


const QUOTED_GLOB_MARKER: char = '\u{1}';

struct Fields {
    done: Vec<(String, bool)>,
    current: String,
    started: bool,
    glob: bool,
    split: bool,
    whitespace: Vec<char>,
    separators: Vec<char>,
}

impl Fields {
    fn new(ifs: &str) -> Self {
        let mut whitespace = Vec::new();
        let mut separators = Vec::new();
        for separator in ifs.chars() {
            if separator.is_whitespace() {
                whitespace.push(separator);
            } else {
                separators.push(separator);
            }
        }
        Fields {
            done: Vec::new(),
            current: String::new(),
            started: false,
            glob: false,
            split: !ifs.is_empty(),
            whitespace,
            separators,
        }
    }

    // Literal source text is not subject to field splitting: only expansion
    // results are.
    fn literal(&mut self, text: &str) {
        for char in text.chars() {
            self.current.push(char);
            self.started = true;
            self.mark_glob(char);
        }
    }

    fn mark_glob(&mut self, char: char) {
        if matches!(char, '*' | '?' | '[' | '@' | '+' | '!' | '(') {
            self.glob = true;
        }
    }

    fn push(&mut self, text: &str, quoted: bool) {
        if quoted {
            self.push_quoted(text);
            return;
        }

        if !self.split {
            self.literal(text);
            return;
        }

        let chars: Vec<char> = text.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            let char = chars[index];
            if self.whitespace.contains(&char) {
                let mut next = index + 1;
                while next < chars.len() && self.whitespace.contains(&chars[next]) {
                    next += 1;
                }
                // IFS whitespace glued to a non-whitespace separator belongs to
                // that separator; on its own it only terminates a started field.
                let glued = chars
                    .get(next)
                    .is_some_and(|char| self.separators.contains(char));
                if !glued {
                    self.flush();
                }
                index = next;
                continue;
            }
            if self.separators.contains(&char) {
                // A non-whitespace separator always ends a field, producing an
                // empty one when nothing had started since the last delimiter.
                if self.started {
                    self.flush();
                } else {
                    self.done.push((String::new(), false));
                }
                index += 1;
                while index < chars.len() && self.whitespace.contains(&chars[index]) {
                    index += 1;
                }
                continue;
            }
            self.current.push(char);
            self.started = true;
            self.mark_glob(char);
            index += 1;
        }
    }

    fn push_quoted(&mut self, text: &str) {
        if text.is_empty() {
            self.started = true;
            return;
        }
        for char in text.chars() {
            if is_glob_meta(char) {
                self.current.push(QUOTED_GLOB_MARKER);
            }
            self.current.push(char);
            self.started = true;
        }
    }

    fn field_quoted_multi(&mut self, values: &[String], join: Option<String>) {
        if values.is_empty() {
            if join.is_some() {
                self.started = true;
            }
            return;
        }
        if let Some(separator) = join {
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    self.current.push_str(&separator);
                }
                self.push_quoted(value);
            }
            return;
        }
        let last = values.len() - 1;
        for (index, value) in values.iter().enumerate() {
            self.push_quoted(value);
            if index < last {
                self.flush();
            }
        }
    }

    fn field_multi(&mut self, values: &[String], join: Option<String>) {
        if values.is_empty() {
            return;
        }
        if let Some(separator) = join {
            let joined = values.join(&separator);
            self.push(&joined, false);
            return;
        }
        let last = values.len() - 1;
        for (index, value) in values.iter().enumerate() {
            self.push(value, false);
            if index < last {
                self.flush();
            }
        }
    }

    fn touch(&mut self) {
        self.started = true;
    }

    fn flush(&mut self) {
        if !self.started {
            return;
        }
        self.done
            .push((std::mem::take(&mut self.current), self.glob));
        self.started = false;
        self.glob = false;
    }

    fn finish(mut self) -> Vec<(String, bool)> {
        if self.started {
            self.flush();
        }
        self.done
    }
}

fn has_glob(text: &str) -> bool {
    has_glob_opts(text, true)
}

fn has_glob_opts(text: &str, extglob: bool) -> bool {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == QUOTED_GLOB_MARKER {
            let _ = chars.next();
            continue;
        }
        if matches!(ch, '*' | '?') {
            return true;
        }
        if extglob && matches!(ch, '@' | '+' | '!') && chars.peek() == Some(&'(') {
            return true;
        }
        if ch == '[' {
            let mut scan = chars.clone();
            if matches!(scan.peek(), Some(&'!' | &'^')) {
                let _ = scan.next();
            }
            let mut saw_elem = false;
            while let Some(inner) = scan.next() {
                if inner == QUOTED_GLOB_MARKER {
                    let _ = scan.next();
                    saw_elem = true;
                    continue;
                }
                if inner == '/' {
                    break;
                }
                if inner == ']' && saw_elem {
                    return true;
                }
                saw_elem = true;
            }
        }
    }
    false
}

fn is_glob_meta(char: char) -> bool {
    matches!(char, '*' | '?' | '[' | ']' | '@' | '+' | '!' | '(' | ')')
}

fn strip_quoted_glob_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(char) = chars.next() {
        if char != QUOTED_GLOB_MARKER {
            out.push(char);
        } else if let Some(literal) = chars.next() {
            out.push(literal);
        }
    }
    out
}

fn glob_with_opts(pattern: &str, globstar: bool, dotglob: bool, extglob: bool) -> Vec<String> {
    if globstar && pattern_has_globstar(pattern) {
        return globstar_glob_traced_opts(pattern, &mut GlobStats::default(), dotglob, extglob);
    }
    let trimmed = pattern.trim_end_matches('/');
    let trailing_slashes = &pattern[trimmed.len()..];
    if trimmed.is_empty() {
        return Vec::new();
    }
    let dirs_only = !trailing_slashes.is_empty();

    let (dir, base) = match trimmed.rfind('/') {
        Some(index) => (&trimmed[..index], &trimmed[index + 1..]),
        None => (".", trimmed),
    };
    if base.is_empty() {
        return Vec::new();
    }

    let directory = if dir.is_empty() { "/" } else { dir };
    let directories = if has_glob_opts(directory, extglob) {
        glob_with_opts(directory, globstar, dotglob, extglob)
    } else {
        vec![strip_quoted_glob_markers(directory)]
    };

    let mut matches = Vec::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !base.starts_with('.') && !dotglob {
                continue;
            }
            if !glob_match_opts(base, &name, extglob, false) {
                continue;
            }
            if dirs_only && !entry.path().is_dir() {
                continue;
            }
            let mut path = match trimmed.rfind('/') {
                Some(_) => format!("{}/{name}", directory.trim_end_matches('/')),
                None => name,
            };
            if dirs_only {
                path.push_str(trailing_slashes);
            }
            matches.push(path);
        }
    }

    matches.sort();
    matches
}

fn component_is_globstar(component: &str) -> bool {
    let mut stars = 0;
    for char in component.chars() {
        if char == QUOTED_GLOB_MARKER || char != '*' {
            return false;
        }
        stars += 1;
    }
    stars == 2
}

fn pattern_has_globstar(pattern: &str) -> bool {
    pattern.split('/').any(component_is_globstar)
}

fn gs_join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else if prefix.ends_with('/') {
        format!("{prefix}{name}")
    } else {
        format!("{prefix}/{name}")
    }
}

fn gs_dir(prefix: &str) -> &str {
    if prefix.is_empty() { "." } else { prefix }
}

fn gs_slash(prefix: &str) -> String {
    if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

fn gs_is_dir(entry: &std::fs::DirEntry, follow_symlink: bool) -> bool {
    entry
        .file_type()
        .map(|kind| kind.is_dir() || (follow_symlink && kind.is_symlink() && entry.path().is_dir()))
        .unwrap_or(false)
}

#[derive(Default)]
struct GlobStats {
    expanded: HashSet<(String, usize)>,
    entry_visits: HashSet<String>,
    #[cfg(test)]
    attempts: Vec<(String, usize)>,
    #[cfg(test)]
    skipped: usize,
    #[cfg(test)]
    entries_skipped: usize,
}

#[allow(clippy::too_many_arguments)]
fn gs_walk_opts(
    prefix: &str,
    components: &[&str],
    descend_at: &[bool],
    index: usize,
    trailing: bool,
    bare_run_multiple: bool,
    literal_prefix: bool,
    dotglob: bool,
    extglob: bool,
    stats: &mut GlobStats,
    out: &mut BTreeSet<String>,
) {
    let key = (prefix.to_string(), index);
    #[cfg(test)]
    stats.attempts.push(key.clone());
    if !stats.expanded.insert(key) {
        #[cfg(test)]
        {
            stats.skipped += 1;
        }
        return;
    }

    let Some(component) = components.get(index) else {
        if !prefix.is_empty() {
            out.insert(prefix.to_string());
        }
        return;
    };

    if component_is_globstar(component) {
        if index + 1 == components.len() {
            if !prefix.is_empty() && Path::new(gs_dir(prefix)).is_dir() {
                if trailing || (literal_prefix && !bare_run_multiple) {
                    out.insert(gs_slash(prefix));
                } else {
                    out.insert(prefix.to_string());
                }
            }
            gs_entries_opts(prefix, trailing, dotglob, stats, out);
            return;
        }
        let descend = descend_at[index];
        gs_walk_opts(
            prefix,
            components,
            descend_at,
            index + 1,
            trailing,
            bare_run_multiple,
            literal_prefix,
            dotglob,
            extglob,
            stats,
            out,
        );
        // Bash 5.3.20: a symlinked dir is followed only for the next component, not for deeper ** levels.
        let Ok(entries) = std::fs::read_dir(gs_dir(prefix)) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if (name.starts_with('.') && !dotglob) || !gs_is_dir(&entry, descend) {
                continue;
            }
            let resume = if gs_is_dir(&entry, false) { index } else { index + 1 };
            gs_walk_opts(
                &gs_join(prefix, &name),
                components,
                descend_at,
                resume,
                trailing,
                bare_run_multiple,
                literal_prefix,
                dotglob,
                extglob,
                stats,
                out,
            );
        }
        return;
    }

    let Ok(entries) = std::fs::read_dir(gs_dir(prefix)) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !component.starts_with('.') && !dotglob {
            continue;
        }
        if !glob_match_opts(component, &name, extglob, false) {
            continue;
        }
        gs_walk_opts(
            &gs_join(prefix, &name),
            components,
            descend_at,
            index + 1,
            trailing,
            bare_run_multiple,
            literal_prefix,
            dotglob,
            extglob,
            stats,
            out,
        );
    }
}

/// How `history` matches a pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchMode {
    Contains,
    Prefix,
    Exact,
}

/// Parsed `history` search options. Defaults are the documented surface: a
/// case-insensitive substring match, newest first, newline separated.
struct HistoryOptions {
    max: Option<usize>,
    reverse: bool,
    show_time: bool,
    null: bool,
    case_sensitive: bool,
    mode: MatchMode,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        HistoryOptions {
            max: None,
            reverse: false,
            show_time: false,
            null: false,
            case_sensitive: false,
            mode: MatchMode::Contains,
        }
    }
}

/// The timestamp line `history --show-time` prints before one entry, in UTC.
///
/// The sandbox has no timezone database and a shell should not depend on one to
/// print history, so the format is deliberately the unambiguous UTC form
/// `# YYYY-MM-DD HH:MM:SS`. `when` is microseconds since the Unix epoch.
fn format_when(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        day_seconds / 3600,
        (day_seconds % 3600) / 60,
        day_seconds % 60
    )
}

/// Howard Hinnant's `civil_from_days`: the proleptic Gregorian date for a day
/// count since 1970-01-01. Pure arithmetic, so the conversion needs no crate.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

fn gs_entries_opts(
    prefix: &str,
    trailing: bool,
    dotglob: bool,
    stats: &mut GlobStats,
    out: &mut BTreeSet<String>,
) {
    if !stats.entry_visits.insert(prefix.to_string()) {
        #[cfg(test)]
        {
            stats.entries_skipped += 1;
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(gs_dir(prefix)) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !dotglob {
            continue;
        }
        let real_dir = gs_is_dir(&entry, false);
        let path = gs_join(prefix, &name);
        if trailing {
            if gs_is_dir(&entry, true) {
                out.insert(gs_slash(&path));
            }
        } else {
            out.insert(path.clone());
        }
        if real_dir {
            gs_entries_opts(&path, trailing, dotglob, stats, out);
        }
    }
}

// Consecutive ** share one walk instead of re-traversing the tree and deduping afterwards.
fn collapse_globstars(
    components: Vec<&str>,
    descend_flags: Vec<bool>,
    trailing: bool,
) -> (Vec<&str>, Vec<bool>, bool) {
    let mut parts = Vec::new();
    let mut descend_at = Vec::new();
    let mut bare_run_multiple = false;
    let mut index = 0;
    while index < components.len() {
        if component_is_globstar(components[index]) {
            let mut run = 1;
            while index + run < components.len() && component_is_globstar(components[index + run]) {
                run += 1;
            }
            bare_run_multiple = index + run == components.len() && !trailing && run > 1;
            parts.push(components[index]);
            descend_at.push(descend_flags[index]);
            index += run;
        } else {
            parts.push(components[index]);
            descend_at.push(descend_flags[index]);
            index += 1;
        }
    }
    (parts, descend_at, bare_run_multiple)
}

#[cfg(test)]
fn globstar_glob_traced(pattern: &str, stats: &mut GlobStats) -> Vec<String> {
    globstar_glob_traced_opts(pattern, stats, false, true)
}

fn globstar_glob_traced_opts(
    pattern: &str,
    stats: &mut GlobStats,
    dotglob: bool,
    extglob: bool,
) -> Vec<String> {
    let absolute = pattern.starts_with('/');
    let trailing = pattern.ends_with('/');
    let mut components: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let mut descend_flags: Vec<bool> = components
        .iter()
        .enumerate()
        .map(|(index, part)| component_is_globstar(part) && index > 0)
        .collect();
    let mut dot_prefix = String::new();
    while components
        .first()
        .is_some_and(|part| *part == "." || *part == "..")
    {
        let part = components.remove(0);
        descend_flags.remove(0);
        if !dot_prefix.is_empty() {
            dot_prefix.push('/');
        }
        dot_prefix.push_str(part);
    }
    if !dot_prefix.is_empty() {
        dot_prefix.push('/');
    }
    let (components, descend_at, bare_run_multiple) =
        collapse_globstars(components, descend_flags, trailing);
    let literal_prefix = components
        .last()
        .is_some_and(|part| component_is_globstar(part))
        && components[..components.len() - 1]
            .iter()
            .all(|part| !has_glob(part));
    let prefix = if absolute { "/".to_string() } else { dot_prefix };
    let mut matches = BTreeSet::new();
    gs_walk_opts(
        &prefix,
        &components,
        &descend_at,
        0,
        trailing,
        bare_run_multiple,
        literal_prefix,
        dotglob,
        extglob,
        stats,
        &mut matches,
    );
    matches.into_iter().collect()
}

fn trim_prefix_opts(
    value: &str,
    pattern: &str,
    longest: bool,
    extglob: bool,
    nocasematch: bool,
) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut indices: Vec<usize> = (0..=chars.len()).collect();
    if longest {
        indices.reverse();
    }

    for index in indices {
        let prefix: String = chars[..index].iter().collect();
        if glob_match_opts(pattern, &prefix, extglob, nocasematch) {
            return chars[index..].iter().collect();
        }
    }

    value.to_string()
}

fn trim_suffix_opts(
    value: &str,
    pattern: &str,
    longest: bool,
    extglob: bool,
    nocasematch: bool,
) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut indices: Vec<usize> = (0..=chars.len()).collect();
    if !longest {
        indices.reverse();
    }

    for index in indices {
        let suffix: String = chars[index..].iter().collect();
        if glob_match_opts(pattern, &suffix, extglob, nocasematch) {
            return chars[..index].iter().collect();
        }
    }

    value.to_string()
}

fn substitute_pattern(
    value: &str,
    pattern: &str,
    replacement: &str,
    all: bool,
    extglob: bool,
    nocasematch: bool,
) -> String {
    if let Some(pat) = pattern.strip_prefix('#') {
        let chars: Vec<char> = value.chars().collect();
        for end in (0..=chars.len()).rev() {
            let slice: String = chars[..end].iter().collect();
            if glob_match_opts(pat, &slice, extglob, nocasematch) {
                let rest: String = chars[end..].iter().collect();
                return format!("{replacement}{rest}");
            }
        }
        return value.to_string();
    }
    if let Some(pat) = pattern.strip_prefix('%') {
        let chars: Vec<char> = value.chars().collect();
        for start in 0..=chars.len() {
            let slice: String = chars[start..].iter().collect();
            if glob_match_opts(pat, &slice, extglob, nocasematch) {
                let head: String = chars[..start].iter().collect();
                return format!("{head}{replacement}");
            }
        }
        return value.to_string();
    }

    let chars: Vec<char> = value.chars().collect();
    let mut out = String::new();
    let mut pos = 0;
    while pos <= chars.len() {
        let mut matched_end = None;
        for end in (pos..=chars.len()).rev() {
            if pos == end && pos == chars.len() && !all {
                break;
            }
            let slice: String = chars[pos..end].iter().collect();
            if glob_match_opts(pattern, &slice, extglob, nocasematch) {
                matched_end = Some(end);
                break;
            }
        }
        if let Some(end) = matched_end {
            out.push_str(replacement);
            if !all {
                for ch in &chars[end..] {
                    out.push(*ch);
                }
                return out;
            }
            if end == pos {
                if pos == chars.len() {
                    break;
                }
                out.push(chars[pos]);
                pos += 1;
            } else {
                pos = end;
            }
        } else {
            if pos == chars.len() {
                break;
            }
            out.push(chars[pos]);
            pos += 1;
        }
    }
    out
}

#[derive(Clone)]
enum BraceItem {
    Lit(char),
    Part(Part),
}

fn brace_expand_word(word: &Word) -> Option<Vec<Word>> {
    if !word
        .parts
        .iter()
        .any(|part| matches!(part, Part::Literal(text) if text.contains('{')))
    {
        return None;
    }

    let mut items = Vec::new();
    for part in &word.parts {
        match part {
            Part::Literal(text) => items.extend(text.chars().map(BraceItem::Lit)),
            other => items.push(BraceItem::Part(other.clone())),
        }
    }

    let expanded = brace_expand_items(&items)?;
    Some(
        expanded
            .into_iter()
            .map(|alts| expand_leading_tilde(items_to_word(&alts)))
            .collect(),
    )
}

fn brace_expand_items(items: &[BraceItem]) -> Option<Vec<Vec<BraceItem>>> {
    for open in 0..items.len() {
        if !matches!(items[open], BraceItem::Lit('{')) {
            continue;
        }
        let mut depth = 0usize;
        let mut close = None;
        for (pos, item) in items.iter().enumerate().skip(open) {
            match item {
                BraceItem::Lit('{') => depth += 1,
                BraceItem::Lit('}') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close = Some(pos);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            continue;
        };

        let prefix = &items[..open];
        let inner = &items[open + 1..close];
        let suffix = &items[close + 1..];
        let parts = split_brace_alternatives(inner);

        let alternatives: Vec<Vec<BraceItem>> = if parts.len() > 1 {
            parts
        } else if inner.iter().all(|item| matches!(item, BraceItem::Lit(_))) {
            let inner_text: String = inner
                .iter()
                .filter_map(|item| match item {
                    BraceItem::Lit(ch) => Some(*ch),
                    BraceItem::Part(_) => None,
                })
                .collect();
            let Some(values) = range_values(&inner_text) else {
                continue;
            };
            values
                .into_iter()
                .map(|value| value.chars().map(BraceItem::Lit).collect())
                .collect()
        } else {
            continue;
        };

        let mut out = Vec::new();
        for alternative in alternatives {
            let mut candidate = Vec::with_capacity(prefix.len() + alternative.len() + suffix.len());
            candidate.extend_from_slice(prefix);
            candidate.extend(alternative);
            candidate.extend_from_slice(suffix);
            match brace_expand_items(&candidate) {
                Some(nested) => out.extend(nested),
                None => out.push(candidate),
            }
            if out.len() > 1024 {
                return None;
            }
        }
        return Some(out);
    }

    None
}

fn split_brace_alternatives(items: &[BraceItem]) -> Vec<Vec<BraceItem>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;

    for (index, item) in items.iter().enumerate() {
        match item {
            BraceItem::Lit('{') => depth += 1,
            BraceItem::Lit('}') => depth = depth.saturating_sub(1),
            BraceItem::Lit(',') if depth == 0 => {
                parts.push(items[start..index].to_vec());
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(items[start..].to_vec());
    parts
}

fn items_to_word(items: &[BraceItem]) -> Word {
    let mut parts = Vec::new();
    let mut literal = String::new();
    for item in items {
        match item {
            BraceItem::Lit(ch) => literal.push(*ch),
            BraceItem::Part(part) => {
                if !literal.is_empty() {
                    parts.push(Part::Literal(std::mem::take(&mut literal)));
                }
                parts.push(part.clone());
            }
        }
    }
    if !literal.is_empty() {
        parts.push(Part::Literal(literal));
    }
    Word { parts }
}

/// Resolve a leading unquoted `~` revealed after brace expansion (e.g. `{~,foo}`).
fn expand_leading_tilde(mut word: Word) -> Word {
    let Some(Part::Literal(text)) = word.parts.first() else {
        return word;
    };
    if !text.starts_with('~') {
        return word;
    }
    let chars: Vec<char> = text.chars().collect();
    let Some((len, tilde)) = crate::lexer::scan_tilde(&chars) else {
        return word;
    };
    let touches_expansion = len == chars.len()
        && word
            .parts
            .get(1)
            .is_some_and(|part| !matches!(part, Part::Literal(_)));
    let ended_at_slash = chars[len - 1] == '/';
    if touches_expansion && !ended_at_slash {
        return word;
    }
    let rest: String = chars[len..].iter().collect();
    word.parts[0] = tilde;
    if !rest.is_empty() {
        word.parts.insert(1, Part::Literal(rest));
    }
    word
}

fn is_zero_padded(token: &str) -> bool {
    let digits = token
        .strip_prefix('-')
        .or_else(|| token.strip_prefix('+'))
        .unwrap_or(token);
    digits.len() > 1 && digits.starts_with('0')
}

fn range_values(inner: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = inner.split("..").collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }

    let explicit = if parts.len() == 3 {
        Some(parts[2].parse::<i64>().ok()?)
    } else {
        None
    };

    if let (Ok(first), Ok(last)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let padded = is_zero_padded(parts[0]) || is_zero_padded(parts[1]);
        let width = if padded {
            parts[0]
                .trim_start_matches('+')
                .len()
                .max(parts[1].trim_start_matches('+').len())
        } else {
            0
        };
        let mut step = explicit.unwrap_or(if first <= last { 1 } else { -1 });
        if step == 0 {
            return None;
        }
        if first > last {
            step = -step.abs();
        }
        let mut values = Vec::new();
        let mut value = first;
        while (step > 0 && value <= last) || (step < 0 && value >= last) {
            if padded {
                values.push(format!("{value:0width$}"));
            } else {
                values.push(value.to_string());
            }
            if values.len() > 10_000 {
                return None;
            }
            value = value.checked_add(step)?;
        }
        return Some(values);
    }

    let mut first = parts[0].chars();
    let start = first.next()?;
    if first.next().is_some() {
        return None;
    }
    let mut last = parts[1].chars();
    let end = last.next()?;
    if last.next().is_some() {
        return None;
    }

    let mut step = explicit.unwrap_or(if start <= end { 1 } else { -1 });
    if step == 0 {
        return None;
    }
    if start > end {
        step = -step.abs();
    }
    let mut values = Vec::new();
    let mut value = start as i64;
    let end = end as i64;
    while (step > 0 && value <= end) || (step < 0 && value >= end) {
        values.push(char::from_u32(value as u32)?.to_string());
        if values.len() > 10_000 {
            return None;
        }
        value = value.checked_add(step)?;
    }
    Some(values)
}

pub(crate) fn glob_match_opts(
    pattern: &str,
    text: &str,
    extglob: bool,
    nocasematch: bool,
) -> bool {
    let pattern: Vec<char> = if nocasematch {
        pattern.to_lowercase().chars().collect()
    } else {
        pattern.chars().collect()
    };
    let text: Vec<char> = if nocasematch {
        text.to_lowercase().chars().collect()
    } else {
        text.chars().collect()
    };
    glob_chars_opts(&pattern, &text, extglob)
}

fn glob_chars(pattern: &[char], text: &[char]) -> bool {
    glob_chars_opts(pattern, text, true)
}

fn glob_chars_opts(pattern: &[char], text: &[char], extglob: bool) -> bool {
    if extglob && let Some((operator, alternatives, rest)) = split_extglob(pattern) {
        for consumed in 0..=text.len() {
            if ext_matches(operator, &alternatives, &text[..consumed])
                && glob_chars_opts(rest, &text[consumed..], extglob)
            {
                return true;
            }
        }
        return false;
    }

    let Some(&first) = pattern.first() else {
        return text.is_empty();
    };

    match first {
        '\u{1}' => {
            let Some(&literal) = pattern.get(1) else {
                return false;
            };
            !text.is_empty()
                && literal == text[0]
                && glob_chars_opts(&pattern[2..], &text[1..], extglob)
        }
        '*' => {
            let mut index = 0;
            loop {
                if glob_chars_opts(&pattern[1..], &text[index..], extglob) {
                    return true;
                }
                if index == text.len() {
                    return false;
                }
                index += 1;
            }
        }
        '?' => !text.is_empty() && glob_chars_opts(&pattern[1..], &text[1..], extglob),
        '[' => {
            let Some(&value) = text.first() else {
                return false;
            };
            let mut index = 1;
            let negate = pattern
                .get(index)
                .is_some_and(|char| *char == '!' || *char == '^');
            if negate {
                index += 1;
            }
            let mut matched = false;
            let mut first_member = true;

            while index < pattern.len() && (pattern[index] != ']' || first_member) {
                first_member = false;
                let (start_char, after_start) =
                    if pattern[index] == QUOTED_GLOB_MARKER && index + 1 < pattern.len() {
                        (pattern[index + 1], index + 2)
                    } else {
                        (pattern[index], index + 1)
                    };
                if after_start + 1 < pattern.len()
                    && pattern[after_start] == '-'
                    && pattern[after_start + 1] != ']'
                {
                    let (end_char, after_end) = if pattern[after_start + 1] == QUOTED_GLOB_MARKER
                        && after_start + 2 < pattern.len()
                    {
                        (pattern[after_start + 2], after_start + 3)
                    } else {
                        (pattern[after_start + 1], after_start + 2)
                    };
                    if start_char <= value && value <= end_char {
                        matched = true;
                    }
                    index = after_end;
                } else {
                    if start_char == value {
                        matched = true;
                    }
                    index = after_start;
                }
            }

            if index >= pattern.len() {
                return false;
            }
            matched != negate && glob_chars_opts(&pattern[index + 1..], &text[1..], extglob)
        }
        char => {
            !text.is_empty()
                && char == text[0]
                && glob_chars_opts(&pattern[1..], &text[1..], extglob)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stated_tree() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sharky-gs-states-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("d1/sub")).unwrap();
        std::fs::create_dir_all(dir.join("d2")).unwrap();
        std::fs::write(dir.join("d1/a.txt"), "").unwrap();
        std::fs::write(dir.join("d1/sub/b.txt"), "").unwrap();
        std::fs::write(dir.join("d2/c.txt"), "").unwrap();
        dir
    }

    #[test]
    fn globstar_expands_each_state_once() {
        let dir = stated_tree();
        let cases = [
            ("**/*.txt", 0, 0),
            ("**/**/*.txt", 0, 0),
            ("**/*/**/*.txt", 1, 0),
            ("**/**", 0, 0),
            ("**/*/**", 0, 1),
            ("**/*/**/", 0, 1),
        ];
        for (suffix, expected_skipped, expected_entries_skipped) in cases {
            let pattern = format!("{}/{suffix}", dir.display());
            let mut stats = GlobStats::default();
            let out = globstar_glob_traced(&pattern, &mut stats);
            let unique: HashSet<_> = stats.attempts.iter().cloned().collect();
            assert_eq!(stats.expanded.len(), unique.len(), "{suffix}");
            assert_eq!(
                stats.skipped,
                stats.attempts.len() - unique.len(),
                "{suffix}"
            );
            assert_eq!(stats.skipped, expected_skipped, "{suffix}");
            assert_eq!(stats.entries_skipped, expected_entries_skipped, "{suffix}");
            let mut sorted = out.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(out, sorted, "{suffix}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // The three backslash dialects differ only where bash's do.

    #[test]
    fn escape_dialects_split_where_bash_splits_them() {
        let cases: &[(&[u8], Escapes, &[u8], bool)] = &[
            (br"a\123b", Escapes::Echo, br"a\123b", false),
            (br"a\123b", Escapes::PercentB, b"aSb", false),
            (br"a\123b", Escapes::Format, b"aSb", false),
            (br"a\0101b", Escapes::Echo, b"aAb", false),
            (br"a\0101b", Escapes::PercentB, b"aAb", false),
            (br"a\0101b", Escapes::Format, b"a\x081b", false),
            (br"a\08b", Escapes::Echo, b"a\x008b", false),
            (br"a\cb", Escapes::Echo, b"a", true),
            (br"a\cb", Escapes::PercentB, b"a", true),
            (br"a\cb", Escapes::Format, br"a\cb", false),
            (br"a\'b", Escapes::Format, b"a'b", false),
            (br"a\'b", Escapes::Echo, br"a\'b", false),
            (br"a\x41b", Escapes::Format, b"aAb", false),
            (br"a\x", Escapes::Format, br"a\x", false),
            (br"a\xZZb", Escapes::Echo, br"a\xZZb", false),
            (br"a\u41b", Escapes::Format, b"a\xd0\x9b", false),
            (br"a\U0001F600b", Escapes::PercentB, b"a\xf0\x9f\x98\x80b", false),
            (br"a\uZZZZb", Escapes::Echo, br"a\uZZZZb", false),
            (br"a\101b", Escapes::Echo, br"a\101b", false),
            (br"a\7b", Escapes::Echo, br"a\7b", false),
            (br"trailing\\", Escapes::Format, br"trailing\", false),
        ];
        for (input, dialect, expected, stopped) in cases {
            let (bytes, stop) = decode_escapes(input, *dialect, false);
            assert_eq!(bytes, *expected, "{input:?} {dialect:?}");
            assert_eq!(stop, *stopped, "{input:?} {dialect:?}");
        }
    }

    #[test]
    fn form_percent_leaves_the_conversion_to_the_parser() {
        // bash prints the backslash and rereads the `%`, so `\%b` is a `%b`.
        let (outcome, next) = decode_escape(b"\\%b", 1, Escapes::Format, false);
        assert!(matches!(outcome, Escape::Rescan), "the % must be reread");
        assert_eq!(next, 1);
    }

    // `%q`'s control-byte form is unreachable through the shell today: the
    // expansion path drops control bytes before printf sees them, so the branch
    // is covered here instead.

    #[test]
    fn quote_for_reuse_matches_the_forms_bash_prints() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b"''"),
            (b"a b", br"a\ b"),
            (b"it's", br"it\'s"),
            (b"a*b", br"a\*b"),
            (b"~x", br"\~x"),
            (b"a~b", b"a~b"),
            (b"#x", br"\#x"),
            (b"a#b", b"a#b"),
            (b"a.b-c_d/e:f@g+h=i%j", b"a.b-c_d/e:f@g+h=i%j"),
            (b"a,b", br"a\,b"),
            (b"a\\b", br"a\\b"),
            (b"\xc3\xa7", b"\xc3\xa7"),
            (b"a\nb", b"$'a\\nb'"),
            (b"a\tb", b"$'a\\tb'"),
            (b"a\rb", b"$'a\\rb'"),
            (b"a\x1bb", b"$'a\\Eb'"),
            (b"a\x01b", b"$'a\\001b'"),
            (b"a\x7fb", b"$'a\\177b'"),
            (b"a b\nc", b"$'a b\\nc'"),
            (b"a\\b\tc", b"$'a\\\\b\\tc'"),
            (b"a'b\tc", b"$'a\\'b\\tc'"),
        ];
        for (input, expected) in cases {
            assert_eq!(quote_for_reuse(input), *expected, "{input:?}");
        }
    }

    #[test]
    fn unicode_escapes_encode_like_bash() {
        let cases: &[(u32, &[u8])] = &[
            (0x41, b"A"),
            (0x00e9, b"\xc3\xa9"),
            (0x041b, b"\xd0\x9b"),
            (0x20ac, b"\xe2\x82\xac"),
            (0x1_f600, b"\xf0\x9f\x98\x80"),
            (0x11_0000, b"\xf4\x90\x80\x80"),
            (0x7fff_ffff, b"\xfd\xbf\xbf\xbf\xbf\xbf"),
            (0xffff_ffff, b""),
        ];
        for (value, expected) in cases {
            let mut out = Vec::new();
            push_utf8(*value, &mut out);
            assert_eq!(out, *expected, "{value:#x}");
        }
    }

    fn history_scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sharky-history-paths-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn history_paths_records_only_what_exists_at_record_time() {
        let dir = history_scratch("existing");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("note.txt"), b"x").unwrap();

        let paths = Shell::history_paths("cat note.txt sub missing.txt", &dir);
        assert_eq!(paths, vec!["note.txt".to_string(), "sub".to_string()]);

        // A word that is not a path is not metadata, and neither is a redirect
        // target that has not been created yet.
        let paths = Shell::history_paths("printf 'X' > note.txt", &dir);
        assert_eq!(paths, vec!["note.txt".to_string()]);

        let paths = Shell::history_paths("mkdir fresh-dir", &dir);
        assert!(paths.is_empty(), "recorded {paths:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn history_paths_keeps_at_most_the_codec_limit() {
        let dir = history_scratch("limit");
        for index in 0..12 {
            std::fs::write(dir.join(format!("f{index}")), b"x").unwrap();
        }
        let line: String = (0..12)
            .map(|index| format!("f{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let paths = Shell::history_paths(&line, &dir);
        assert_eq!(paths.len(), crate::history::MAX_PATHS);
        assert_eq!(paths[0], "f0");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn history_paths_resolves_absolute_words_as_they_are() {
        let dir = history_scratch("absolute");
        let file = dir.join("abs.txt");
        std::fs::write(&file, b"x").unwrap();
        let line = format!("cat {}", file.display());
        let paths = Shell::history_paths(&line, &dir);
        assert_eq!(paths, vec![file.display().to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// The session surface the agent runtime drives: evaluation without the
/// per-input-unit finish, read-only state snapshots, request-scoped process
/// groups, and the `exec` refusal. See `docs/agent-runtime-mvp-design.md`.
#[cfg(test)]
mod agent_surface_tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sharky-agent-surface-{}-{}-{}",
            std::process::id(),
            name,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Set in the dedicated process [`fork_safely`] starts for one test.
    const FORK_SAFE_CHILD: &str = "SHARKY_SHELL_FORK_SAFE_CHILD";

    /// How many threads this process has, from `/proc/self/status`.
    fn thread_count() -> Option<usize> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("Threads:"))
            .and_then(|count| count.trim().parse().ok())
    }

    /// The libtest name of `test`, which is what `--exact` matches.
    fn test_path(test: &str) -> String {
        // `module_path!` carries the crate name; libtest leaves it out.
        let module = module_path!();
        let module = module
            .strip_prefix(concat!(env!("CARGO_PKG_NAME"), "::"))
            .unwrap_or(module);
        format!("{module}::{test}")
    }

    /// Runs `test` again in a fresh process where no other test runs next to
    /// it, and reports whether this copy of the test only had to watch that
    /// child.
    ///
    /// `fork()` copies the calling thread alone, so a child forked from a
    /// libtest worker inherits every lock the other workers held at that
    /// instant — the first `println!`, allocation or capture write then waits
    /// forever on a mutex whose owner does not exist in the child. sharky is
    /// single threaded, so production never meets this; a test binary running
    /// tests side by side does. The body therefore runs in a process started
    /// with `--test-threads 1`, the same shape `tests/agent_capture.rs` uses
    /// for its descriptor surgery.
    ///
    /// The re-run is unconditional: libtest runs every test on a thread of its
    /// own, so a thread count cannot tell a harness that may fork from one that
    /// may not, and the extra process costs milliseconds.
    fn fork_safely(test: &str) -> bool {
        if std::env::var_os(FORK_SAFE_CHILD).is_some() {
            // This is the dedicated child: the body runs here.
            return false;
        }
        let name = test_path(test);
        let (status, stdout, stderr) = run_test_binary(
            &["--exact", &name, "--test-threads", "1", "--nocapture"],
            |command| {
                command.env(FORK_SAFE_CHILD, "1");
            },
        );
        // A `--exact` filter that matches nothing exits 0 too, so insist that
        // the child really ran this one test.
        assert!(
            status.success() && stdout.contains("1 passed"),
            "{name} must pass in a single-threaded child: {status}\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        true
    }

    /// Runs this test binary with `args` and returns its output. A child that
    /// outlives the watchdog is killed and reported as a failure, so a broken
    /// guard fails the suite instead of hanging it.
    fn run_test_binary(
        args: &[&str],
        configure: impl FnOnce(&mut std::process::Command),
    ) -> (std::process::ExitStatus, String, String) {
        use std::io::Read;

        let exe = std::env::current_exe().expect("path of the running test binary");
        let mut command = std::process::Command::new(exe);
        command
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().expect("start the test-binary child");

        // The whole fork suite takes a second; a minute means a deadlock.
        let timeout = std::time::Duration::from_secs(60);
        let deadline = std::time::Instant::now() + timeout;
        let mut timed_out = false;
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll the test-binary child") {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                timed_out = true;
                let _ = child.kill();
                break child.wait().expect("reap the killed test-binary child");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        child
            .stdout
            .take()
            .expect("stdout pipe")
            .read_to_string(&mut stdout)
            .ok();
        child
            .stderr
            .take()
            .expect("stderr pipe")
            .read_to_string(&mut stderr)
            .ok();
        assert!(
            !timed_out,
            "a child run of this test binary hung for {timeout:?} and was killed: {args:?}\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        (status, stdout, stderr)
    }

    /// Panics unless this process may `fork()`.
    ///
    /// The child runs with `--test-threads 1`. libtest still keeps one thread
    /// of its own there, but it only waits for this test, so the process never
    /// has a second worker that could hold a lock across the `fork()`.
    fn assert_may_fork() {
        assert!(
            std::env::var_os(FORK_SAFE_CHILD).is_some(),
            "in_child forked from the multithreaded harness: \
             start the test with `if fork_safely(..) {{ return; }}`"
        );
        if let Some(threads) = thread_count() {
            assert!(
                threads <= 2,
                "a forking test must not share its process with other test \
                 workers, but this one has {threads} threads"
            );
        }
    }

    /// Runs `action` in a forked child whose stdout and stderr point at fresh
    /// files. Any descriptor the shell opens or redirects stays private to the
    /// child, which matters in a test binary whose threads share a descriptor
    /// table and whose harness writes progress lines to stdout.
    ///
    /// Callers must run [`fork_safely`] first: a child forked from a worker
    /// thread can inherit a lock the child can never release.
    fn in_child(
        shell: &mut Shell,
        dir: &std::path::Path,
        action: impl FnOnce(&mut Shell) -> i32,
    ) -> (i32, String, String) {
        use std::os::fd::AsRawFd;

        assert_may_fork();
        let stdout_path = dir.join("child.stdout");
        let stderr_path = dir.join("child.stderr");
        let stdout_file = std::fs::File::create(&stdout_path).expect("create stdout capture");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr capture");
        let stdout_fd = stdout_file.as_raw_fd();
        let stderr_fd = stderr_file.as_raw_fd();

        let mask = crate::signal::block_all();
        // SAFETY: the child runs only shell code and then exits.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                drop(stdout_file);
                drop(stderr_file);
                let status = loop {
                    match waitpid(child, None) {
                        Ok(WaitStatus::Exited(_, code)) => break code,
                        Ok(WaitStatus::Signaled(_, signal, _)) => break 128 + signal as i32,
                        Ok(_) => continue,
                        Err(Errno::EINTR) => continue,
                        Err(err) => panic!("waitpid failed: {err}"),
                    }
                };
                let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
                let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
                (status, stdout, stderr)
            }
            Ok(ForkResult::Child) => {
                crate::signal::restore_mask(mask);
                // SAFETY: the child owns these descriptors until it exits.
                unsafe {
                    nix::libc::dup2(stdout_fd, 1);
                    nix::libc::dup2(stderr_fd, 2);
                }
                let code = action(shell);
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
                unsafe { _exit(code) }
            }
            Err(err) => panic!("fork failed: {err}"),
        }
    }

    #[test]
    fn session_eval_returns_the_command_status_and_keeps_parse_errors() {
        let mut shell = Shell::new();
        assert_eq!(shell.session_eval("true").unwrap(), 0);
        assert_eq!(shell.session_eval("false").unwrap(), 1);
        let err = shell.session_eval("if").unwrap_err();
        assert!(!err.message.is_empty(), "the caller sees the parse error");
    }

    #[test]
    fn session_eval_defers_the_exit_trap_to_finish() {
        if fork_safely("session_eval_defers_the_exit_trap_to_finish") {
            return;
        }
        let dir = scratch("exit-trap");
        let flag = dir.join("flag");
        let mut shell = Shell::new();
        let action = format!("printf a >> '{}'", flag.display());
        let trap_source = format!("trap '{action}' EXIT");

        let (status, _, stderr) = in_child(&mut shell, &dir, |shell| {
            if !matches!(shell.session_eval(&trap_source), Ok(0)) {
                return 11;
            }
            if !matches!(shell.session_eval("X=1"), Ok(0))
                || !matches!(shell.session_eval("X=2"), Ok(0))
            {
                return 12;
            }
            if flag.exists() || shell.exit_trap_ran {
                return 13;
            }
            if shell.finish(3) != 3 {
                return 14;
            }
            // A second finish must not run the action again.
            if shell.finish(4) != 4 {
                return 15;
            }
            0
        });

        assert_eq!(status, 0, "child reported a session_eval/finish failure: {stderr}");
        assert_eq!(
            std::fs::read_to_string(&flag).unwrap(),
            "a",
            "finish runs the EXIT action exactly once"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_env_is_sorted_and_matches_the_state_json_path() {
        let mut shell = Shell::new();
        shell.session_eval("SNAP_A=1; SNAP_B=2").unwrap();
        let env = shell.snapshot_env();
        let mut sorted = env.clone();
        sorted.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(env, sorted, "the snapshot is sorted by name");
        assert_eq!(
            env.iter().find(|(name, _)| name == "SNAP_A").cloned(),
            Some(("SNAP_A".to_string(), "1".to_string()))
        );
    }

    #[test]
    fn snapshot_variables_carries_value_export_and_readonly() {
        let mut shell = Shell::new();
        shell
            .session_eval("SNAP_PLAIN=hi; export SNAP_EX=9; readonly SNAP_RO=1; export SNAP_NOVAL")
            .unwrap();
        let vars = shell.snapshot_variables();
        let find = |name: &str| {
            vars.iter()
                .find(|(key, _, _, _)| key == name)
                .cloned()
        };

        assert_eq!(
            find("SNAP_PLAIN"),
            Some(("SNAP_PLAIN".to_string(), Some("hi".to_string()), false, false))
        );
        assert_eq!(
            find("SNAP_EX"),
            Some(("SNAP_EX".to_string(), Some("9".to_string()), true, false))
        );
        assert_eq!(
            find("SNAP_RO"),
            Some(("SNAP_RO".to_string(), Some("1".to_string()), false, true))
        );
        assert_eq!(
            find("SNAP_NOVAL"),
            Some(("SNAP_NOVAL".to_string(), None, true, false)),
            "an exported name without a value has no value"
        );

        let names: Vec<&String> = vars.iter().map(|(name, _, _, _)| name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted, "one sorted entry per name");
    }

    #[test]
    fn snapshot_arrays_preserves_unset_holes_and_leaves_variables_alone() {
        let mut shell = Shell::new();
        shell.session_eval("SNAP_LIST=(a); SNAP_LIST[2]=c").unwrap();
        let arrays = shell.snapshot_arrays();
        assert_eq!(
            arrays.get("SNAP_LIST"),
            Some(&vec![
                Some("a".to_string()),
                None,
                Some("c".to_string())
            ]),
            "an unset element stays a hole, not the string null"
        );
        assert!(
            shell
                .snapshot_variables()
                .iter()
                .all(|(name, _, _, _)| name != "SNAP_LIST"),
            "arrays are reported separately"
        );
    }

    #[test]
    fn snapshot_assoc_keeps_every_key() {
        let mut shell = Shell::new();
        shell
            .session_eval("declare -A SNAP_MAP=([b]=2 [a]=1)")
            .unwrap();
        let maps = shell.snapshot_assoc();
        let map = maps.get("SNAP_MAP").expect("the associative array exists");
        assert_eq!(
            map.iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect::<Vec<_>>(),
            vec![("a", "1"), ("b", "2")]
        );
    }

    #[test]
    fn snapshot_functions_aliases_and_traps_are_sorted_names() {
        let mut shell = Shell::new();
        shell
            .session_eval("snap_fn(){ :; }; alias snapalias='echo hi'; trap 'echo t' USR1")
            .unwrap();
        assert_eq!(shell.snapshot_functions(), vec!["snap_fn".to_string()]);
        assert_eq!(
            shell.snapshot_aliases(),
            vec![("snapalias".to_string(), "echo hi".to_string())]
        );
        assert_eq!(
            shell.snapshot_traps(),
            vec![("USR1".to_string(), "echo t".to_string())]
        );
    }

    #[test]
    fn snapshot_options_reflect_set_flags() {
        let mut shell = Shell::new();
        assert_eq!(
            shell.snapshot_options(),
            vec![
                ("errexit", false),
                ("nounset", false),
                ("pipefail", false),
                ("noclobber", false),
                ("xtrace", false),
                ("histexpand", true),
            ]
        );
        shell.session_eval("set -e; set -o pipefail; set -o noclobber").unwrap();
        assert_eq!(
            shell.snapshot_options(),
            vec![
                ("errexit", true),
                ("nounset", false),
                ("pipefail", true),
                ("noclobber", true),
                ("xtrace", false),
                ("histexpand", true),
            ]
        );
    }

    #[test]
    fn env_delta_reports_changes_and_removals_sorted() {
        let mut shell = Shell::new();
        shell.session_eval("SNAP_GONE=1; SNAP_KEEP=same").unwrap();
        let before = shell.snapshot_env();
        shell
            .session_eval("SNAP_KEEP=changed; SNAP_NEW=2; unset SNAP_GONE")
            .unwrap();

        let delta = shell.env_delta(&before);
        assert_eq!(
            delta,
            vec![
                ("SNAP_GONE".to_string(), None),
                ("SNAP_KEEP".to_string(), Some("changed".to_string())),
                ("SNAP_NEW".to_string(), Some("2".to_string())),
            ],
            "removals are null, changes carry the value, and `_` stays out"
        );
    }

    #[test]
    fn isolate_groups_defaults_off_and_never_takes_the_terminal() {
        let mut shell = Shell::new();
        assert!(!shell.isolate_groups());
        assert!(!shell.foreground_control());
        shell.set_isolate_groups(true);
        assert!(shell.isolate_groups());
        assert!(
            !shell.foreground_control(),
            "an isolated group is not a terminal hand-off"
        );
        shell.set_isolate_groups(false);
        assert!(!shell.isolate_groups());
    }

    #[test]
    fn exec_is_refused_in_session_mode() {
        if fork_safely("exec_is_refused_in_session_mode") {
            return;
        }
        let dir = scratch("exec-refused");
        let mut shell = Shell::new();
        assert!(!shell.session_mode());
        shell.set_session_mode(true);
        assert!(shell.session_mode());

        let (status, stdout, stderr) = in_child(&mut shell, &dir, |shell| {
            shell.session_eval("exec /bin/true").unwrap_or(-1)
        });
        assert_eq!(status, 126, "stdout={stdout:?} stderr={stderr:?}");
        assert_eq!(
            stderr,
            "sharky: exec: process replacement is not available in agent session mode\n"
        );
        assert_eq!(stdout, "", "the process was not replaced");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exec_redirect_only_still_applies_in_session_mode() {
        if fork_safely("exec_redirect_only_still_applies_in_session_mode") {
            return;
        }
        let dir = scratch("exec-redirect");
        let path = dir.join("data.txt");
        let mut shell = Shell::new();
        shell.set_session_mode(true);
        let source = format!("exec 3>'{}'; printf hi >&3", path.display());
        let (status, _, stderr) = in_child(&mut shell, &dir, |shell| {
            shell.session_eval(&source).unwrap_or(-1)
        });
        assert_eq!(status, 0, "the redirect-only exec must not be refused: {stderr}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enter_task_child_sets_the_task_state_and_leads_its_own_group() {
        if fork_safely("enter_task_child_sets_the_task_state_and_leads_its_own_group") {
            return;
        }
        let dir = scratch("enter-task");
        let mut shell = Shell::new();
        assert!(!shell.session_mode(), "the test starts outside a session");

        let (status, _, stderr) = in_child(&mut shell, &dir, |shell| {
            let session_group = getpgrp();
            shell.enter_task_child();
            if shell.interactive || !shell.in_subshell || !shell.session_mode() {
                return 11;
            }
            if !shell.isolate_groups() || shell.foreground_control() {
                return 12;
            }
            let own = getpid();
            if getpgrp() != own {
                return 13;
            }
            if getpgrp() == session_group {
                return 14;
            }
            if shell.job_pgid != Some(own) {
                return 15;
            }
            0
        });
        assert_eq!(status, 0, "task child state check failed: {stderr}");
        assert!(!shell.session_mode(), "the parent keeps its own state");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn background_job_of_a_task_stays_in_the_task_process_group() {
        if fork_safely("background_job_of_a_task_stays_in_the_task_process_group") {
            return;
        }
        let dir = scratch("task-bg-group");
        let mut shell = Shell::new();
        let (status, _, stderr) = in_child(&mut shell, &dir, |shell| {
            shell.enter_task_child();
            let task_pgid = getpgrp();

            // A task body of `sleep 30 & wait`: start the job, check its group,
            // then reap it through the shell's own `wait`. The sleeper is killed
            // first so the test never spends 30 seconds waiting for it.
            if !matches!(shell.session_eval("sleep 30 &"), Ok(0)) {
                return 11;
            }
            let Some(child) = shell.last_bg_pid else {
                return 12;
            };
            let child_pgid = nix::unistd::getpgid(Some(child)).map(|pgid| pgid.as_raw());

            // Clean up on every path, including a failing group check: a red
            // run must not leave a `sleep 30` orphan behind.
            let _ = nix::sys::signal::kill(child, Signal::SIGKILL);
            let waited = shell.session_eval("wait");

            // `enter_task_child` made the task shell the leader of the group
            // that `cancel` signals. A `&` job started inside it must join that
            // group instead of breaking out into one of its own.
            if child_pgid != Ok(task_pgid.as_raw()) {
                return 13;
            }
            if !matches!(waited, Ok(0)) {
                return 14;
            }
            0
        });
        assert_eq!(status, 0, "background job left the task group: {stderr}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn task_child_runs_its_exit_trap_once_and_leaves_the_session_action_alone() {
        if fork_safely("task_child_runs_its_exit_trap_once_and_leaves_the_session_action_alone") {
            return;
        }
        let dir = scratch("task-exit");
        let own = dir.join("own");
        let session = dir.join("session");
        let mut shell = Shell::new();
        let session_source = format!("trap 'printf s >> {}' EXIT", session.display());
        assert_eq!(shell.session_eval(&session_source).unwrap(), 0);

        // A task that sets no action of its own keeps the session's snapshot
        // unchanged, which is what tells the driver to skip `finish`: the
        // session's EXIT trap must not fire once per task.
        let (status, _, stderr) = in_child(&mut shell, &dir, |shell| {
            let inherited = shell.snapshot_exit_trap();
            if inherited.is_none() {
                return 11;
            }
            shell.enter_task_child();
            if shell.snapshot_exit_trap() != inherited || shell.exit_trap_ran {
                return 12;
            }
            if !matches!(shell.session_eval("printf hi"), Ok(0)) {
                return 13;
            }
            if shell.snapshot_exit_trap() != inherited || shell.exit_trap_ran {
                return 14;
            }
            // The driver's own decision: an unchanged action is never the
            // task's, so `finish` must not run.
            if shell.finish_task(0, inherited.as_deref()) != 0 || shell.exit_trap_ran {
                return 15;
            }
            0
        });
        assert_eq!(status, 0, "{stderr}");

        // A task that sets its own action runs it exactly once, at task end.
        let (status, _, stderr) = in_child(&mut shell, &dir, |shell| {
            shell.enter_task_child();
            let source = format!("trap 'printf o >> {}' EXIT", own.display());
            if !matches!(shell.session_eval(&source), Ok(0)) {
                return 21;
            }
            if own.exists() {
                return 22; // session_eval defers the action to the task end
            }
            if shell.finish(7) != 7 {
                return 23;
            }
            if shell.finish(0) != 0 {
                return 24;
            }
            0
        });
        assert_eq!(status, 0, "{stderr}");
        assert_eq!(
            std::fs::read_to_string(&own).unwrap(),
            "o",
            "the task's own EXIT action runs exactly once"
        );
        assert!(
            !session.exists(),
            "the session's EXIT action must not fire per task"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn task_child_sees_the_session_snapshot_and_never_mutates_it() {
        if fork_safely("task_child_sees_the_session_snapshot_and_never_mutates_it") {
            return;
        }
        let dir = scratch("task-snapshot");
        let mut shell = Shell::new();
        shell.set_var("X", "1".to_string());
        shell.set_var("Y", "2".to_string());
        shell.exported.insert("Y".to_string());
        let pwd = shell.var("PWD");

        let (status, stdout, stderr) = in_child(&mut shell, &dir, |shell| {
            shell.enter_task_child();
            if !matches!(
                shell.session_eval("printf '%s|%s|%s\\n' \"$X\" \"$Y\" \"$PWD\""),
                Ok(0)
            ) {
                return 11;
            }
            if !matches!(shell.session_eval("X=99; export Y=3; cd /"), Ok(0)) {
                return 12;
            }
            if shell.var("PWD") != "/" {
                return 13;
            }
            0
        });
        assert_eq!(status, 0, "{stderr}");
        assert_eq!(stdout, format!("1|2|{pwd}\n"), "the child sees the snapshot");
        assert_eq!(shell.var("X"), "1", "the task's assignment stays local");
        assert_eq!(shell.var("Y"), "2", "the task's export stays local");
        assert_eq!(shell.var("PWD"), pwd, "the task's cd stays local");
        assert!(shell.exit_requested.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn task_child_still_refuses_the_exec_builtin() {
        if fork_safely("task_child_still_refuses_the_exec_builtin") {
            return;
        }
        let dir = scratch("task-exec");
        let mut shell = Shell::new();
        let (status, stdout, stderr) = in_child(&mut shell, &dir, |shell| {
            shell.enter_task_child();
            shell.session_eval("exec /bin/true").unwrap_or(-1)
        });
        assert_eq!(status, 126, "stdout={stdout:?} stderr={stderr:?}");
        assert_eq!(
            stderr,
            "sharky: exec: process replacement is not available in agent session mode\n"
        );
        assert_eq!(stdout, "", "the process was not replaced");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn task_child_evaluates_like_a_command_string_not_the_sessions_errexit() {
        if fork_safely("task_child_evaluates_like_a_command_string_not_the_sessions_errexit") {
            return;
        }
        let dir = scratch("task-errexit");
        let mut shell = Shell::new();
        shell.errexit = true;
        let (status, stdout, stderr) = in_child(&mut shell, &dir, |shell| {
            shell.enter_task_child();
            if shell.errexit || shell.errexit_suppressed {
                return 11;
            }
            match shell.session_eval("false; printf after") {
                Ok(0) => 0,
                _ => 12,
            }
        });
        assert_eq!(status, 0, "{stderr}");
        assert_eq!(stdout, "after", "an inherited errexit does not abort the task");
        assert!(shell.errexit, "the parent keeps its option");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fork_safely_reruns_every_forking_test_on_one_thread() {
        // The guard has to be exercised, not assumed: run the forking tests
        // side by side in one harness, which is the shape that hung the whole
        // suite for half an hour. Each must come back after re-running itself
        // where it cannot share the process with the other workers, and the
        // tripwire inside `in_child` fails any of them that forgets.
        let names: Vec<String> = [
            "background_job_of_a_task_stays_in_the_task_process_group",
            "enter_task_child_sets_the_task_state_and_leads_its_own_group",
            "exec_is_refused_in_session_mode",
            "exec_redirect_only_still_applies_in_session_mode",
            "session_eval_defers_the_exit_trap_to_finish",
            "task_child_evaluates_like_a_command_string_not_the_sessions_errexit",
            "task_child_runs_its_exit_trap_once_and_leaves_the_session_action_alone",
            "task_child_sees_the_session_snapshot_and_never_mutates_it",
            "task_child_still_refuses_the_exec_builtin",
        ]
        .iter()
        .map(|test| test_path(test))
        .collect();
        let mut args: Vec<&str> = vec!["--exact"];
        args.extend(names.iter().map(String::as_str));
        args.extend(["--test-threads", "8", "--nocapture"]);
        let (status, stdout, stderr) = run_test_binary(&args, |command| {
            command.env_remove(FORK_SAFE_CHILD);
        });
        assert!(
            status.success() && stdout.contains("9 passed; 0 failed"),
            "{names:?} must pass under a multithreaded harness once each re-runs itself: \
             {status}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
    }
}
