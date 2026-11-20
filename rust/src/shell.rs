use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use nix::fcntl::OFlag;
use nix::libc::_exit;
use nix::sys::signal::{SigHandler, Signal, killpg, signal};
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
use crate::test_expr;
use crate::z::Z;
use crate::lexer::{self, ParamOp, Parameter, Part, TokenKind, Word};
use crate::parser;

pub struct Shell {
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub last_status: i32,
    pub last_duration_ms: u64,
    funcs: HashMap<String, List>,
    arrays: HashMap<String, Vec<Option<String>>>,
    assoc: HashMap<String, BTreeMap<String, String>>,
    positional: Vec<String>,
    aliases: HashMap<String, String>,
    abbreviations: HashMap<String, String>,
    wake: Vec<String>,
    keybinds: HashMap<String, String>,
    settings: HashMap<String, String>,
    script: Option<String>,
    // Set only for a `-c` command string. Bash reports a null or unset
    // parameter error with 127 there, but with 1 when the same text arrives as
    // a script file, on standard input, in a subshell or interactively.
    command_string: bool,
    interactive: bool,
    errexit: bool,
    nounset: bool,
    xtrace: bool,
    pipefail: bool,
    errexit_suppressed: bool,
    nounset_failed: bool,
    // ${name?word} and ${name:?word} failures, which bash reports with 127 for
    // a -c command string but with 1 in a script, a subshell, an interactive
    // shell or a shell with errexit enabled.
    null_parameter_error: bool,
    in_subshell: bool,
    traps: HashMap<String, String>,
    exit_trap_ran: bool,
    history: Option<History>,
    history_load_error: Option<String>,
    histexpand: bool,
    vi: bool,
    globstar: bool,
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
    next_job: usize,
    job_pgid: Option<Pid>,
    last_bg_pid: Option<Pid>,
    colors: HashMap<String, String>,
    styles: HashMap<String, String>,
    local_scopes: Vec<Vec<SavedLocal>>,
}

struct SavedLocal {
    name: String,
    var: Option<String>,
    array: Option<Vec<Option<String>>>,
    assoc: Option<BTreeMap<String, String>>,
}

struct Job {
    id: usize,
    pgid: Pid,
    pids: Vec<Pid>,
    command: String,
    stopped: bool,
}

const SIGNALS: &[&str] = &[
    "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV", "USR2",
    "PIPE", "ALRM", "TERM", "CHLD", "CONT", "STOP", "TSTP", "TTIN", "TTOU", "URG", "XCPU",
    "XFSZ", "VTALRM", "PROF", "WINCH", "IO", "SYS",
];

enum Flow {
    Status(i32),
    Exit(i32),
    // An interactive expansion error abandons the rest of the input unit, as
    // bash discards the line it is executing, without leaving the shell. It
    // unwinds to `run_program`; a forked child maps it to its exit status.
    Discard(i32),
    Break,
    Continue,
    Return(i32),
}

struct Prepared {
    argv: Vec<String>,
    actions: Vec<Action>,
    env: Vec<(String, String)>,
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
    program: Option<&'a Path>,
}

enum Action {
    Write(i32, PathBuf),
    Append(i32, PathBuf),
    Read(i32, PathBuf),
    Dup { from: i32, to: i32 },
    Here { fd: i32, file: std::fs::File },
}

impl Action {
    // The descriptor this action moves and, for a dup, the one it copies.
    fn fds(&self) -> (RawFd, Option<RawFd>) {
        match self {
            Action::Write(fd, _)
            | Action::Append(fd, _)
            | Action::Read(fd, _)
            | Action::Here { fd, .. } => (*fd, None),
            Action::Dup { from, to } => (*to, Some(*from)),
        }
    }
}

impl Shell {
    pub fn new() -> Self {
        let env = std::env::vars_os()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    String::from_utf8_lossy(&value.into_vec()).into_owned(),
                )
            })
            .collect();

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut shell = Shell {
            env,
            cwd: cwd.clone(),
            last_status: 0,
            last_duration_ms: 0,
            funcs: HashMap::new(),
            arrays: HashMap::new(),
            assoc: HashMap::new(),
            positional: Vec::new(),
            aliases: HashMap::new(),
            abbreviations: HashMap::new(),
            wake: Vec::new(),
            keybinds: HashMap::new(),
            settings: HashMap::new(),
            script: None,
            command_string: false,
            interactive: false,
            errexit: false,
            nounset: false,
            xtrace: false,
            pipefail: false,
            errexit_suppressed: false,
            nounset_failed: false,
            null_parameter_error: false,
            in_subshell: false,
            traps: HashMap::new(),
            exit_trap_ran: false,
            history: None,
            history_load_error: None,
            histexpand: true,
            vi: false,
            globstar: false,
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
            jobs: Vec::new(),
            next_job: 1,
            job_pgid: None,
            last_bg_pid: None,
            colors: HashMap::new(),
            styles: HashMap::new(),
            local_scopes: Vec::new(),
        };
        shell.set_var("PWD", cwd.display().to_string());
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

    fn apply_config(&mut self, config: Config) {
        for (name, value) in config.env {
            self.set_var(&name, value);
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
        self.run(&source)
    }

    pub fn run(&mut self, source: &str) -> i32 {
        let status = match self.eval(source) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("sharky: {}", err.message);
                2
            }
        };
        let status = self.finish(status);
        self.close_substitutions_from(0);
        status
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

        let saved = self.exit_requested;
        self.exit_requested = None;
        let _ = self.run(&command);
        if let Some(code) = self.exit_requested.take() {
            return code;
        }
        self.exit_requested = saved;
        status
    }

    pub fn eval(&mut self, source: &str) -> Result<i32, parser::ParseError> {
        self.eval_with(source, || {})
    }

    pub fn eval_with(
        &mut self,
        source: &str,
        before_run: impl FnOnce(),
    ) -> Result<i32, parser::ParseError> {
        let program = parser::parse(source)?;
        before_run();
        Ok(match self.run_program(&program) {
            Flow::Status(code) | Flow::Exit(code) | Flow::Discard(code) | Flow::Return(code) => code,
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
        names
    }

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

    pub fn command_words(&self) -> Vec<String> {
        let mut words: Vec<String> = [
            ":", "true", "false", "cd", "exit", "export", "unset", "set", "source", ".",
            "break", "continue", "return", "jobs", "fg", "bg", "declare", "local", "pwd", "type",
            "alias", "abbr", "unabbr", "unalias", "history", "wait", "kill", "z", "zi",
            "reload", "trap", "read", "shift", "wake", "exec", "time", "shopt",
            "mapfile", "readarray", "command", "help", "echo", "printf",
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
                // The input unit ends here. The shell keeps its state for the
                // next one; only the status survives, for `$?`.
                Flow::Discard(code) => {
                    self.last_status = code;
                    return Flow::Discard(code);
                }
                other => return other,
            }
        }

        Flow::Status(status)
    }

    fn run_list(&mut self, list: &List) -> Flow {
        let mut status = self.last_status;

        for entry in &list.entries {
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
        }

        Flow::Status(status)
    }

    fn run_background(&mut self, and_or: &AndOr) {
        // SAFETY: the child runs shell code and exits without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                let _ = setpgid(child, child);
                let id = self.next_job;
                self.next_job += 1;
                self.last_bg_pid = Some(child);
                println!("[{id}] {child}");
                self.jobs.push(Job {
                    id,
                    pgid: child,
                    pids: vec![child],
                    command: describe_and_or(and_or),
                    stopped: false,
                });
            }
            Ok(ForkResult::Child) => {
                let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
                reset_child_signals();
                self.job_pgid = Some(getpid());
                let code = flow_status(self.run_and_or(and_or));
                unsafe { _exit(code) }
            }
            Err(err) => {
                eprintln!("sharky: fork: {err}");
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
            let status = match &flow {
                Flow::Status(code) => *code,
                _ => {
                    self.errexit_suppressed = previous;
                    return flow;
                }
            };
            let should_run = if *is_and { status == 0 } else { status != 0 };
            if should_run {
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

            // SAFETY: the child runs shell code and exits without unwinding.
            match unsafe { fork() } {
                Ok(ForkResult::Parent { child }) => {
                    if self.interactive {
                        if leader.is_none() {
                            self.set_foreground(child);
                            leader = Some(child);
                        } else if let Some(leader) = leader {
                            let _ = setpgid(child, leader);
                        }
                    }
                    pids.push(child);
                }
                Ok(ForkResult::Child) => {
                    if self.interactive {
                        let target = self.job_pgid.or(leader).unwrap_or(Pid::from_raw(0));
                        let _ = setpgid(Pid::from_raw(0), target);
                    }
                    reset_child_signals();
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
                    let code = self.run_command_child(command);
                    unsafe { _exit(code) }
                }
                Err(err) => {
                    eprintln!("sharky: fork: {err}");
                    return Flow::Status(1);
                }
            }
        }

        drop(pipes);

        let command = describe_pipeline(pipeline);
        let pgid = leader.unwrap_or_else(getpgrp);
        let status = self.wait_pipeline(&pids, pgid, command);
        self.reap_substitutions();
        self.errexit_check(Flow::Status(status))
    }

    fn run_command(&mut self, command: &Command) -> Flow {
        match command {
            Command::Simple(simple) => self.run_simple(simple),
            Command::Not(inner) => match self.run_command(inner) {
                Flow::Status(code) => Flow::Status(if code == 0 { 1 } else { 0 }),
                other => other,
            },
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
            Command::Function { name, body } => {
                self.funcs.insert(name.clone(), body.clone());
                Flow::Status(0)
            }
            Command::Redirected {
                command,
                redirects,
            } => self.run_redirected(command, redirects),
            Command::Test(words) => self.run_test(words),
            Command::Arithmetic(source) => Flow::Status(match arith::evaluate(source, self) {
                Ok(0) => 1,
                Ok(_) => 0,
                Err(message) => {
                    eprintln!("sharky: arithmetic: {message}");
                    2
                }
            }),
        }
    }

    fn run_test(&mut self, words: &[Word]) -> Flow {
        let mut expand = |word: &Word| self.expand_text_word(word);
        let result = test_expr::evaluate(words, &mut expand);
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

    fn run_simple(&mut self, simple: &Simple) -> Flow {
        let substitutions = self.substitution_fds.len();
        let flow = self.run_simple_inner(simple);
        self.close_substitutions_from(substitutions);
        flow
    }

    fn run_simple_inner(&mut self, simple: &Simple) -> Flow {
        if simple.words.is_empty() {
            return self.run_assignments(simple);
        }

        let mut prepared = match self.prepare(simple) {
            Ok(prepared) => prepared,
            Err(message) => {
                if !message.is_empty() {
                    eprintln!("sharky: {message}");
                }
                return self.expansion_failure();
            }
        };

        if self.nounset_failed {
            return self.unset_parameter_failure();
        }

        if let Some(name) = prepared.argv.first()
            && let Some(expansion) = self.aliases.get(name).cloned()
            && let Some(argv) = self.expand_alias(&expansion, &prepared.argv[1..])
        {
            prepared.argv = argv;
        }

        if prepared.argv.first().map(String::as_str) == Some("exec") {
            if !prepared.actions.is_empty()
                && let Err(message) = apply_actions(&prepared.actions)
            {
                eprintln!("sharky: {message}");
                return Flow::Status(1);
            }
            return self.exec_builtin(&prepared.argv[1..], &prepared.env);
        }

        if prepared.argv.is_empty() {
            if prepared.actions.is_empty() {
                return Flow::Status(0);
            }
        } else if self.funcs.contains_key(&prepared.argv[0]) || is_builtin_name(&prepared.argv[0]) {
            let argv = &prepared.argv;
            return self.with_actions(&prepared.actions, |shell| {
                shell.run_in_shell(argv, &prepared.declarations, &prepared.env)
            });
        }

        let command = prepared.argv.join(" ");
        match self.spawn(
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
        }
    }

    // An assignment-only command keeps its assignments in the current shell.
    // Its redirections apply to that command alone, but they are performed here
    // so a failed one reports its status without dropping the assignments,
    // which bash still applies. Assignment words expand before the redirections
    // are applied, so an expansion failure creates and assigns nothing.
    fn run_assignments(&mut self, simple: &Simple) -> Flow {
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
                for assignment in &simple.assignments {
                    self.assign(assignment);
                }
                if let Some(flow) = self.take_expansion_failure() {
                    return flow;
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
        for assignment in &simple.assignments {
            self.assign(assignment);
        }
        let failure = self.take_expansion_failure();
        let substitution = self.substitution_status.take().filter(|code| *code != 0);
        let applied = apply_actions(&actions);
        restore_fds(saved);

        if let Some(flow) = failure {
            return flow;
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

    fn run_in_shell(
        &mut self,
        argv: &[String],
        declarations: &[Assignment],
        env: &[(String, String)],
    ) -> Flow {
        if let Some(body) = self.funcs.get(&argv[0]).cloned() {
            let saved = std::mem::replace(&mut self.positional, argv[1..].to_vec());
            self.local_scopes.push(Vec::new());
            let flow = self.run_list(&body);
            self.restore_locals();
            self.positional = saved;
            return match flow {
                Flow::Return(code) => Flow::Status(code),
                other => other,
            };
        }

        match self.builtin(argv, declarations, env) {
            Some(flow) => flow,
            None => {
                eprintln!("sharky: command not found: {}", argv[0]);
                Flow::Status(127)
            }
        }
    }

    // Runs in a forked child: never forks again for a simple command, so pipe
    // write ends close when the child exits.
    fn run_command_child(&mut self, command: &Command) -> i32 {
        match command {
            Command::Simple(simple) => self.run_simple_child(simple),
            other => flow_status(self.run_command(other)),
        }
    }

    fn run_simple_child(&mut self, simple: &Simple) -> i32 {
        let substitutions = self.substitution_fds.len();
        let status = self.run_simple_child_inner(simple);
        self.close_substitutions_from(substitutions);
        status
    }

    fn run_simple_child_inner(&mut self, simple: &Simple) -> i32 {
        if simple.words.is_empty() && simple.redirects.is_empty() {
            self.substitution_status = None;
            for assignment in &simple.assignments {
                self.assign(assignment);
            }
            if self.nounset_failed {
                return 1;
            }
            return self.substitution_status.take().unwrap_or(0);
        }

        let prepared = match self.prepare(simple) {
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

        if prepared.argv.is_empty() {
            return 0;
        }

        if prepared.argv[0] == "exec" {
            return flow_status(self.exec_builtin(&prepared.argv[1..], &prepared.env));
        }

        if let Some(body) = self.funcs.get(&prepared.argv[0]).cloned() {
            let saved = std::mem::replace(&mut self.positional, prepared.argv[1..].to_vec());
            self.local_scopes.push(Vec::new());
            let flow = self.run_list(&body);
            self.restore_locals();
            self.positional = saved;
            return match flow {
                Flow::Return(code) => code,
                other => flow_status(other),
            };
        }

        if let Some(flow) = self.builtin(&prepared.argv, &prepared.declarations, &prepared.env) {
            return flow_status(flow);
        }

        match exec::resolve(&prepared.argv[0], self) {
            Some(path) => exec::exec_replace(&path, &prepared.argv, &prepared.env),
            None => {
                eprintln!("sharky: command not found: {}", prepared.argv[0]);
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

        self.publish_json_exec();
        match exec::exec_now(&path, &argv, env) {
            Ok(()) => Flow::Status(0),
            Err(err) => {
                eprintln!("sharky: exec: {program}: {err}");
                self.exec_failure(126)
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

    fn run_subshell(&mut self, list: &List) -> Flow {
        // SAFETY: the child runs shell code and exits without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                let status = exec::wait(child);
                self.reclaim_terminal();
                Flow::Status(status)
            }
            Ok(ForkResult::Child) => {
                reset_child_signals();
                self.in_subshell = true;
                let code = flow_status(self.run_list(list));
                unsafe { _exit(code) }
            }
            Err(err) => {
                eprintln!("sharky: fork: {err}");
                Flow::Status(1)
            }
        }
    }

    fn run_redirected(&mut self, command: &Command, redirects: &[Redirect]) -> Flow {
        let actions = match self.redirect_actions(redirects) {
            Ok(actions) => actions,
            Err(message) => {
                eprintln!("sharky: {message}");
                return Flow::Status(1);
            }
        };

        if let Command::Simple(simple) = command
            && simple.words.first().and_then(literal).as_deref() == Some("exec")
        {
            if let Err(message) = apply_actions(&actions) {
                eprintln!("sharky: {message}");
                return Flow::Status(1);
            }
            let prepared = match self.prepare(simple) {
                Ok(prepared) => prepared,
                Err(message) => {
                    eprintln!("sharky: {message}");
                    return Flow::Status(1);
                }
            };
            return self.exec_builtin(&prepared.argv[1..], &prepared.env);
        }

        // A group shares the shell's state, so its redirections are temporary
        // rather than a fork boundary.
        if matches!(command, Command::Group(_)) {
            return self.with_actions(&actions, |shell| shell.run_command(command));
        }

        // SAFETY: the child runs shell code and exits without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                let status = exec::wait(child);
                self.reclaim_terminal();
                Flow::Status(status)
            }
            Ok(ForkResult::Child) => {
                reset_child_signals();
                if let Err(message) = apply_actions(&actions) {
                    eprintln!("sharky: {message}");
                    unsafe { _exit(1) }
                }
                let code = self.run_command_child(command);
                unsafe { _exit(code) }
            }
            Err(err) => {
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
            && let Err(message) = arith::evaluate(init, self)
        {
            eprintln!("sharky: arithmetic: {message}");
            return Flow::Status(2);
        }

        let mut status = 0;
        loop {
            if !condition.is_empty() {
                match arith::evaluate(condition, self) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(message) => {
                        eprintln!("sharky: arithmetic: {message}");
                        return Flow::Status(2);
                    }
                }
            }

            match self.run_list(body) {
                Flow::Status(code) => status = code,
                Flow::Break => break,
                Flow::Continue => {}
                other => return other,
            }

            if !step.is_empty()
                && let Err(message) = arith::evaluate(step, self)
            {
                eprintln!("sharky: arithmetic: {message}");
                return Flow::Status(2);
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
                    if glob_match(&pattern, &value) {
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

        let mut env = self.env.clone();
        let mut declarations = Vec::new();
        let declaring = argv.first().is_some_and(|name| is_declaration_word(name));
        for assignment in &simple.assignments {
            match &assignment.value {
                AssignmentValue::Array(_) if declaring => declarations.push(assignment.clone()),
                AssignmentValue::Array(_) => self.assign(assignment),
                AssignmentValue::Word(word) if assignment.index.is_some() => {
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
            declarations,
        })
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
                        Action::Write(1, path)
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
                        RedirectOp::Out => Action::Write(redirect.fd, path),
                        RedirectOp::Append => Action::Append(redirect.fd, path),
                        RedirectOp::In => Action::Read(redirect.fd, path),
                        RedirectOp::Dup(_)
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
        // SAFETY: the child only runs syscalls and allocation-safe code before exec.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                if leader.is_none() {
                    self.set_foreground(child);
                }
                Ok(child)
            }
            Ok(ForkResult::Child) => {
                if self.interactive {
                    let target = self.job_pgid.or(leader).unwrap_or(Pid::from_raw(0));
                    let _ = setpgid(Pid::from_raw(0), target);
                }
                reset_child_signals();
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
                let resolved = match program {
                    Some(path) => Some(path.to_path_buf()),
                    None => exec::resolve(&argv[0], self),
                };
                match resolved {
                    Some(path) => exec::exec_replace(&path, argv, env),
                    None => {
                        eprintln!("sharky: command not found: {}", argv[0]);
                        unsafe { _exit(127) }
                    }
                }
            }
            Err(err) => Err(format!("fork: {err}")),
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
            "return" => Some(Flow::Return(
                words
                    .get(1)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(self.last_status),
            )),
            "break" => Some(Flow::Break),
            "continue" => Some(Flow::Continue),
            "cd" => Some(Flow::Status(self.cd(words.get(1).map(String::as_str)))),
            "export" => Some(Flow::Status(self.export(&words[1..]))),
            "unset" => Some(Flow::Status(self.unset(&words[1..]))),
            "set" => Some(Flow::Status(self.set(&words[1..]))),
            "shopt" => Some(Flow::Status(self.shopt(&words[1..]))),
            "jobs" => Some(Flow::Status(self.list_jobs())),
            "pwd" => {
                println!("{}", self.cwd.display());
                Some(Flow::Status(0))
            }
            "echo" => Some(Flow::Status(echo_builtin(&words[1..]))),
            "printf" => Some(Flow::Status(self.printf_builtin(&words[1..]))),
            "type" => Some(Flow::Status(self.type_builtin(&words[1..]))),
            "command" => Some(self.command_builtin(&words[1..], env)),
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
            "read" => Some(Flow::Status(self.read(&words[1..]))),
            "mapfile" | "readarray" => Some(Flow::Status(self.mapfile(&words[1..]))),
            "shift" => Some(Flow::Status(self.shift(&words[1..]))),
            "fg" => Some(Flow::Status(self.fg(&words[1..]))),
            "bg" => Some(Flow::Status(self.bg(&words[1..]))),
            "source" | "." => Some(self.source(&words[1..])),
            _ => None,
        }
    }

    fn expansion_var(&mut self, name: &str) -> String {
        if self.nounset
            && !is_special_name(name)
            && !self.arrays.contains_key(name)
            && !self.env.iter().any(|(key, _)| key == name)
        {
            eprintln!("sharky: {name}: unbound variable");
            self.nounset_failed = true;
            return String::new();
        }
        self.var(name)
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
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    fn assign(&mut self, assignment: &Assignment) {
        match &assignment.value {
            AssignmentValue::Array(elements) => {
                let entries = self.expand_array_entries(elements);
                if self.assoc.contains_key(&assignment.name) {
                    let map: BTreeMap<String, String> = assoc_entries(&entries).into_iter().collect();
                    if assignment.append {
                        self.assoc
                            .entry(assignment.name.clone())
                            .or_default()
                            .extend(map);
                    } else {
                        self.assoc.insert(assignment.name.clone(), map);
                    }
                } else {
                    self.assign_array_entries(&assignment.name, &entries, assignment.append);
                }
            }
            AssignmentValue::Word(word) => {
                let value = self.expand_assignment(word);
                let index = assignment
                    .index
                    .as_ref()
                    .map(|index| self.expand_assignment(index));
                if self.assoc.contains_key(&assignment.name) {
                    let key = index.unwrap_or_else(|| "0".to_string());
                    self.assoc
                        .entry(assignment.name.clone())
                        .or_default()
                        .insert(key, value);
                } else {
                    match index {
                        Some(index) => self.set_array_element(&assignment.name, &index, value),
                        None => self.set_var(&assignment.name, value),
                    }
                }
            }
        }
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
    ) {
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
    }

    fn declaration(
        &mut self,
        command: &str,
        args: &[String],
        declarations: &[Assignment],
    ) -> i32 {
        let local = command == "local";
        if local && self.local_scopes.is_empty() {
            eprintln!("sharky: local: can only be used in a function");
            return 1;
        }

        let mut kind = None;
        let mut print = false;
        let mut only_indexed = false;
        let mut only_assoc = false;
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
                    'a' => {
                        kind = Some(false);
                        only_indexed = true;
                    }
                    'A' => {
                        kind = Some(true);
                        only_assoc = true;
                    }
                    'p' => print = true,
                    other => {
                        eprintln!("sharky: {command}: -{other}: unsupported option");
                        return 2;
                    }
                }
            }
            index += 1;
        }

        let names = &args[index..];
        if print {
            if local {
                return self.print_locals(names);
            }
            // The shell tracks no readonly attribute, so `readonly -p` prints
            // nothing while `declare -p` still prints every name it is given.
            if command == "readonly" {
                return 0;
            }
            if names.is_empty() {
                self.print_all_variables(only_indexed, only_assoc);
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
            let fresh = local && self.save_local(name);
            if fresh {
                self.env.retain(|(key, _)| key != name);
                self.arrays.remove(name);
                self.assoc.remove(name);
            }

            let kind = kind.or(match literal.map(|declaration| &declaration.value) {
                Some(AssignmentValue::Array(_)) => Some(false),
                _ => None,
            });

            match kind {
                Some(true) => {
                    if self.arrays.contains_key(name) {
                        eprintln!(
                            "sharky: {command}: {name}: cannot convert indexed to associative array"
                        );
                        status = 1;
                        continue;
                    }
                    if local && !fresh {
                        self.env.retain(|(key, _)| key != name);
                    }
                    self.arrays.remove(name);
                    let entries = match literal.map(|declaration| &declaration.value) {
                        Some(AssignmentValue::Array(elements)) => {
                            Some(self.expand_array_entries(elements))
                        }
                        _ => None,
                    };
                    let append = literal.is_some_and(|declaration| declaration.append);
                    let scalar = self.take_scalar(name);
                    let map = self.assoc.entry(name.to_string()).or_default();
                    match entries {
                        Some(entries) => {
                            if !append {
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
                        continue;
                    }
                    if local && !fresh {
                        self.env.retain(|(key, _)| key != name);
                    }
                    self.assoc.remove(name);
                    let had_array = self.arrays.contains_key(name);
                    match literal.map(|declaration| (&declaration.value, declaration.append)) {
                        Some((AssignmentValue::Array(elements), append)) => {
                            let entries = self.expand_array_entries(elements);
                            if append
                                && !had_array
                                && let Some(previous) = self.take_scalar(name)
                            {
                                self.arrays.insert(name.to_string(), vec![Some(previous)]);
                            }
                            self.assign_array_entries(name, &entries, append);
                        }
                        _ => {
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
        names.sort();
        names.dedup();
        for name in names {
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

    fn declaration_line(&self, name: &str) -> Option<String> {
        // Only the array kind is tracked, so `-a`/`-A` are the only flags that
        // can appear; export and readonly have no stored attribute.
        if let Some(map) = self.assoc.get(name) {
            let mut line = format!("declare -A {name}");
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
            let mut line = format!("declare -a {name}");
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
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| {
            format!("declare -- {name}={}", quote_value(value))
        })
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
            self.env.retain(|(key, _)| key != &saved.name);
            self.arrays.remove(&saved.name);
            self.assoc.remove(&saved.name);
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

    fn set_array(&mut self, name: &str, values: Vec<String>, append: bool) {
        if !append {
            // Replacing the array also drops a scalar of the same name, which
            // bash no longer exports once the name is an array.
            self.take_scalar(name);
            self.arrays
                .insert(name.to_string(), values.into_iter().map(Some).collect());
            return;
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
    }

    fn resolve_array_index(&mut self, text: &str) -> usize {
        if let Ok(index) = text.trim().parse::<usize>() {
            return index;
        }
        arith::evaluate(text, self).unwrap_or(0).max(0) as usize
    }

    fn set_array_element(&mut self, name: &str, index: &str, value: String) {
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
    }

    pub(crate) fn set_var(&mut self, name: &str, value: String) {
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
        match self.env.iter_mut().find(|(key, _)| key == name) {
            Some(entry) => entry.1 = value,
            None => self.env.push((name.to_string(), value)),
        }
    }

    // Removes a scalar binding so a name that became an array is no longer
    // exported with a stale value.
    fn take_scalar(&mut self, name: &str) -> Option<String> {
        let position = self.env.iter().position(|(key, _)| key == name)?;
        Some(self.env.remove(position).1)
    }

    fn export(&mut self, args: &[String]) -> i32 {
        for arg in args {
            if let Some((name, value)) = arg.split_once('=') {
                self.set_var(name, value.to_string());
            }
        }
        0
    }

    fn unset(&mut self, args: &[String]) -> i32 {
        for name in args {
            if let Some((array, index)) = split_array_argument(name) {
                let index = unquote_subscript(&index);
                if let Some(assoc) = self.assoc.get_mut(&array) {
                    assoc.remove(&index);
                    continue;
                }
                if let Some(values) = self.arrays.get_mut(&array) {
                    if let Ok(index) = index.parse::<usize>() {
                        if let Some(slot) = values.get_mut(index) {
                            *slot = None;
                        }
                    }
                    continue;
                }
            }
            self.env.retain(|(key, _)| key != name);
            self.arrays.remove(name);
            self.assoc.remove(name);
        }
        0
    }

    pub fn reap_jobs(&mut self) {
        if self.jobs.is_empty() {
            return;
        }

        let mut finished = Vec::new();
        for (index, job) in self.jobs.iter_mut().enumerate() {
            match exec::wait_scan(job.pids[0]) {
                WaitStatus::Exited(_, _) | WaitStatus::Signaled(_, _, _) => {
                    finished.push(index);
                }
                WaitStatus::Stopped(_, _) if !job.stopped => {
                    job.stopped = true;
                    println!("\n[{}]+  Stopped                 {}", job.id, job.command);
                }
                _ => {}
            }
        }

        for index in finished.into_iter().rev() {
            let job = self.jobs.remove(index);
            if self.interactive {
                println!("[{}]+  Done                    {}", job.id, job.command);
            }
        }
    }

    fn wait_foreground(&mut self, pids: &[Pid], pgid: Pid, command: String) -> i32 {
        self.wait_job(pids, pgid, command, false)
    }

    fn wait_pipeline(&mut self, pids: &[Pid], pgid: Pid, command: String) -> i32 {
        self.wait_job(pids, pgid, command, self.pipefail)
    }

    fn wait_job(&mut self, pids: &[Pid], pgid: Pid, command: String, pipefail: bool) -> i32 {
        let mut status = 0;
        let mut statuses = Vec::new();
        let mut stopped = false;

        for pid in pids {
            let code = match exec::wait_traced(*pid) {
                WaitStatus::Exited(_, code) => code,
                WaitStatus::Signaled(_, signal, _) => 128 + signal as i32,
                WaitStatus::Stopped(_, _) => {
                    stopped = true;
                    128 + Signal::SIGTSTP as i32
                }
                _ => 0,
            };
            statuses.push(code);
            status = code;
        }

        if pipefail {
            status = statuses.iter().rev().find(|code| **code != 0).copied().unwrap_or(0);
        }

        self.reclaim_terminal();

        if stopped && self.interactive {
            let id = self.next_job;
            self.next_job += 1;
            self.jobs.push(Job {
                id,
                pgid,
                pids: pids.to_vec(),
                command: command.clone(),
                stopped: true,
            });
            println!("\n[{id}]+  Stopped                 {command}");
        }

        status
    }

    fn list_jobs(&self) -> i32 {
        let count = self.jobs.len();
        for (index, job) in self.jobs.iter().enumerate() {
            let marker = if index + 1 == count {
                '+'
            } else if index + 2 == count {
                '-'
            } else {
                ' '
            };
            let state = if job.stopped { "Stopped" } else { "Running" };
            println!("[{}]{}  {state:<24}{}", job.id, marker, job.command);
        }
        0
    }

    fn job_index(&self, spec: Option<&String>) -> Option<usize> {
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
        let Some(index) = self.job_index(args.first()) else {
            eprintln!("sharky: fg: no such job");
            return 1;
        };

        let mut job = self.jobs.remove(index);
        job.stopped = false;
        println!("{}", job.command);

        self.set_foreground(job.pgid);
        let _ = killpg(job.pgid, Signal::SIGCONT);

        self.wait_foreground(&job.pids, job.pgid, job.command)
    }

    fn bg(&mut self, args: &[String]) -> i32 {
        let Some(index) = self.job_index(args.first()) else {
            eprintln!("sharky: bg: no such job");
            return 1;
        };

        let job = &mut self.jobs[index];
        job.stopped = false;
        let _ = killpg(job.pgid, Signal::SIGCONT);
        println!("[{}]+ {} &", job.id, job.command);
        0
    }

    fn source(&mut self, args: &[String]) -> Flow {
        let Some(path) = args.first() else {
            eprintln!("sharky: source: filename argument required");
            return Flow::Status(2);
        };

        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(err) => {
                eprintln!("sharky: {path}: {err}");
                return Flow::Status(1);
            }
        };

        let program = match parser::parse(&source) {
            Ok(program) => program,
            Err(err) => {
                eprintln!("sharky: {}: {}", path, err.message);
                return Flow::Status(2);
            }
        };

        let saved = std::mem::replace(&mut self.positional, args[1..].to_vec());
        let flow = self.run_program(&program);
        self.positional = saved;
        // An interactive expansion error inside the file abandons it and
        // reports the status, while the caller's input line keeps running. Bash
        // resumes at the next line of the file too; the parser keeps a file in
        // one list, so this stops at the file boundary instead.
        match flow {
            Flow::Discard(code) => Flow::Status(code),
            other => other,
        }
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

    pub fn push_history(&mut self, line: &str) {
        if let Some(history) = &mut self.history {
            if let Err(err) = history.try_add(line) {
                eprintln!("sharky: {err}");
            }
        }
    }

    pub fn history_entries(&self) -> &[String] {
        self.history
            .as_ref()
            .map(History::entries)
            .unwrap_or(&[])
    }

    fn z_builtin(&mut self, args: &[String], interactive: bool) -> i32 {
        if args.is_empty() {
            return self.cd(None);
        }
        if args.len() == 1 && args[0] == "-" {
            return self.cd(Some("-"));
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
        self.cd(Some(&target))
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
            let jobs = std::mem::take(&mut self.jobs);
            for job in jobs {
                for pid in &job.pids {
                    let _ = exec::wait(*pid);
                }
            }
            for pid in std::mem::take(&mut self.substitution_pids) {
                let _ = exec::wait(pid);
            }
            return 0;
        }

        let target = &args[0];
        if target.starts_with('%') {
            let Some(index) = self.job_index(Some(target)) else {
                eprintln!("sharky: wait: {target}: no such process");
                return 127;
            };
            let job = self.jobs.remove(index);
            let mut status = 0;
            for pid in &job.pids {
                status = exec::wait(*pid);
            }
            return status;
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
            let job = self.jobs.remove(index);
            let mut status = 0;
            for child in &job.pids {
                status = exec::wait(*child);
            }
            return status;
        }

        exec::wait(Pid::from_raw(pid))
    }

    fn kill_builtin(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            eprintln!("sharky: kill: usage: kill [-SIGNAL] %job|pid ...");
            return 2;
        }

        let mut args = args;
        let mut signal = Signal::SIGTERM;
        if let Some(option) = args.first().and_then(|arg| arg.strip_prefix('-')) {
            let Some(parsed) = parse_signal(option) else {
                eprintln!("sharky: kill: invalid signal: {option}");
                return 2;
            };
            signal = parsed;
            args = &args[1..];
        }

        if args.is_empty() {
            eprintln!("sharky: kill: usage: kill [-SIGNAL] %job|pid ...");
            return 2;
        }

        let mut status = 0;
        for target in args {
            if target.starts_with('%') {
                let Some(index) = self.job_index(Some(target)) else {
                    eprintln!("sharky: kill: {target}: no such job");
                    status = 1;
                    continue;
                };
                let pgid = self.jobs[index].pgid;
                if killpg(pgid, signal).is_err() {
                    eprintln!("sharky: kill: {target}: no such process");
                    status = 1;
                }
                continue;
            }

            let Ok(pid) = target.parse::<i32>() else {
                eprintln!("sharky: kill: {target}: no such process");
                status = 1;
                continue;
            };
            if nix::sys::signal::kill(Pid::from_raw(pid), signal).is_err() {
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

        eprintln!("sharky: history: usage: history [-c | merge]");
        1
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
        if matches!(args.first().map(String::as_str), Some("-l" | "-L")) {
            println!("EXIT {}", SIGNALS.join(" "));
            return 0;
        }

        if args.is_empty() {
            let mut entries: Vec<_> = self.traps.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            for (signal, command) in entries {
                println!("trap -- '{command}' {signal}");
            }
            return 0;
        }

        let (action, signals) = if args[0] == "--" {
            if args.len() == 1 {
                return 0;
            }
            (&args[1], &args[2..])
        } else if args[0] == "-" {
            for name in &args[1..] {
                let Some(signal) = normalize_signal(name) else {
                    eprintln!("sharky: trap: invalid signal specification");
                    return 1;
                };
                self.traps.remove(signal);
            }
            return 0;
        } else {
            (&args[0], &args[1..])
        };

        if signals.is_empty() {
            eprintln!("sharky: trap: action required");
            return 1;
        }

        let mut status = 0;
        for name in signals {
            let Some(signal) = normalize_signal(name) else {
                eprintln!("sharky: trap: invalid signal specification");
                status = 1;
                continue;
            };
            self.traps.insert(signal.to_string(), action.clone());
        }
        status
    }

    fn read(&mut self, args: &[String]) -> i32 {
        let raw = args.first().map(String::as_str) == Some("-r");
        let names: Vec<String> = if raw {
            args[1..].to_vec()
        } else {
            args.to_vec()
        };
        // Without names the whole line is stored in REPLY untrimmed; a named
        // REPLY goes through the usual splitting path.
        let implicit_reply = names.is_empty();
        let names = if implicit_reply {
            vec!["REPLY".to_string()]
        } else {
            names
        };

        let mut line = String::new();
        loop {
            let Some(chunk) = read_stdin_line() else {
                return 1;
            };
            line.push_str(&chunk);
            if !raw && line.ends_with("\\\n") {
                line.truncate(line.len() - 2);
                continue;
            }
            break;
        }

        let ended = line.ends_with('\n');
        if ended {
            line.pop();
        }
        let fields = if implicit_reply {
            vec![dequote_read_line(&line, !raw)]
        } else {
            split_read_fields(&line, &self.field_separators(), !raw, names.len())
        };
        for (name, value) in names.iter().zip(fields) {
            self.set_var(name, value);
        }
        if ended { 0 } else { 1 }
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
        while let Some(mut line) = read_stdin_line() {
            if strip && line.ends_with('\n') {
                line.pop();
            }
            values.push(line);
        }
        self.set_array(&name, values, false);
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
        if self.histexpand {
            active.push("histexpand");
        }
        if self.vi {
            active.push("vi");
        }
        active
    }

    fn shopt(&mut self, args: &[String]) -> i32 {
        let mut args = args;
        let on = match args.first().map(String::as_str) {
            Some("-s") => true,
            Some("-u") => false,
            Some(name) => {
                if name == "globstar" {
                    println!("globstar\t{}", if self.globstar { "on" } else { "off" });
                    return i32::from(!self.globstar);
                }
                eprintln!("sharky: shopt: {name}: invalid option");
                return 1;
            }
            None => return 1,
        };
        args = &args[1..];
        for name in args {
            if name != "globstar" {
                eprintln!("sharky: shopt: {name}: invalid option name");
                return 1;
            }
            self.globstar = on;
        }
        0
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

    fn set_named_option(&mut self, name: &str, on: bool) -> bool {
        match name {
            "errexit" => self.errexit = on,
            "nounset" => self.nounset = on,
            "xtrace" => self.xtrace = on,
            "histexpand" => self.histexpand = on,
            "pipefail" => self.pipefail = on,
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

    fn cd(&mut self, target: Option<&str>) -> i32 {
        let target = match target {
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
                old
            }
            Some(path) => path.to_string(),
        };

        let previous = self.cwd.clone();
        match std::env::set_current_dir(&target) {
            Ok(()) => {
                self.cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
                self.set_var("OLDPWD", previous.display().to_string());
                self.set_var("PWD", self.cwd.display().to_string());
                let cwd = self.cwd.display().to_string();
                if self.z.is_none() {
                    self.z = Z::new();
                }
                if let Some(z) = &mut self.z {
                    z.record(&cwd);
                }
                0
            }
            Err(err) => {
                eprintln!("sharky: cd: {target}: {err}");
                1
            }
        }
    }

    fn expand_word(&mut self, word: &Word) -> Vec<String> {
        if let [Part::Literal(text)] = word.parts.as_slice()
            && text.contains('{')
            && let Some(alternatives) = brace_expand(text)
        {
            let mut fields = Vec::new();
            for alternative in alternatives {
                let word = Word {
                    parts: vec![Part::Literal(alternative)],
                };
                fields.extend(self.expand_word(&word));
            }
            return fields;
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
                    Err(message) => {
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
                    Err(message) => {
                        eprintln!("sharky: arithmetic: {message}");
                        self.expansion_error = true;
                    }
                },
                Part::Tilde => out.push_str(&self.var("HOME")),
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
        if let Some(array) = self.arrays.get(&parameter.name) {
            return array
                .iter()
                .enumerate()
                .filter(|(_, value)| value.is_some())
                .map(|(index, _)| index.to_string())
                .collect::<Vec<_>>()
                .join(&separator);
        }
        if let Some(assoc) = self.assoc.get(&parameter.name) {
            return assoc.keys().cloned().collect::<Vec<_>>().join(&separator);
        }
        self.scalar_key(&parameter.name).unwrap_or_default()
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
        let index = parameter.index.as_deref();
        let multiple = matches!(index, Some("@") | Some("*"));

        if parameter.operator == Some(ParamOp::Length) && multiple {
            if let Some(array) = self.arrays.get(&parameter.name) {
                return array.iter().flatten().count().to_string();
            }
            if let Some(assoc) = self.assoc.get(&parameter.name) {
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

        let numeric_index = if self.assoc.contains_key(&parameter.name) {
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
        );
        // Without word splitting the at form joins its elements with a space,
        // whatever IFS holds; the star form keeps the first IFS character.
        let multi_separator = if lookup == Some("@") {
            " ".to_string()
        } else {
            self.ifs_join_separator()
        };
        let value = if let Some(assoc) = self.assoc.get(&parameter.name) {
            match lookup {
                Some("@") | Some("*") => assoc
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(&multi_separator),
                Some(key) => assoc.get(key).cloned().unwrap_or_default(),
                None => assoc.get("0").cloned().unwrap_or_default(),
            }
        } else if let Some(array) = self.arrays.get(&parameter.name) {
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
            self.var(&parameter.name)
        } else {
            self.expansion_var(&parameter.name)
        };
        let argument = parameter.argument.clone().unwrap_or_default();
        let is_numeric = !parameter.name.is_empty()
            && parameter.name.chars().all(|char| char.is_ascii_digit());
        let defined = if let Some(assoc) = self.assoc.get(&parameter.name) {
            match lookup {
                Some("@") | Some("*") => !assoc.is_empty(),
                Some(key) => assoc.contains_key(key),
                None => assoc.contains_key("0"),
            }
        } else if let Some(array) = self.arrays.get(&parameter.name) {
            match lookup {
                Some("@") | Some("*") => array.iter().any(Option::is_some),
                Some(_) => array.get(numeric_index).is_some_and(Option::is_some),
                None => array.first().is_some_and(Option::is_some),
            }
        } else if parameter.name == "0" {
            true
        } else if is_numeric {
            parameter
                .name
                .parse::<usize>()
                .ok()
                .and_then(|index| index.checked_sub(1))
                .is_some_and(|index| index < self.positional.len())
        } else if parameter.name == "@" || parameter.name == "*" {
            !self.positional.is_empty()
        } else if is_special_name(&parameter.name) {
            true
        } else {
            self.env.iter().any(|(key, _)| key == &parameter.name)
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
                    self.set_var(&parameter.name, new.clone());
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
                    self.set_var(&parameter.name, new.clone());
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
            Some(ParamOp::TrimPrefixShort) => trim_prefix(&value, &argument, false),
            Some(ParamOp::TrimPrefixLong) => trim_prefix(&value, &argument, true),
            Some(ParamOp::TrimSuffixShort) => trim_suffix(&value, &argument, false),
            Some(ParamOp::TrimSuffixLong) => trim_suffix(&value, &argument, true),
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
                } else if op == ParamOp::SubstituteAll {
                    value.replace(pattern, replacement)
                } else {
                    value.replacen(pattern, replacement, 1)
                }
            }
        }
    }

    fn parameter_elements(&mut self, parameter: &Parameter) -> Option<Vec<String>> {
        if parameter.index.is_none()
            && parameter.operator.is_none()
            && matches!(parameter.name.as_str(), "@" | "*")
        {
            return Some(self.positional.clone());
        }

        match parameter.operator {
            Some(ParamOp::Keys) => {
                // Only the `@` and `*` subscripts list the keys; a numeric or
                // associative subscript indirects through the element instead.
                if !matches!(parameter.index.as_deref(), Some("@") | Some("*")) {
                    return None;
                }
                if let Some(assoc) = self.assoc.get(&parameter.name) {
                    return Some(assoc.keys().cloned().collect());
                }
                if let Some(array) = self.arrays.get(&parameter.name) {
                    return Some(
                        array
                            .iter()
                            .enumerate()
                            .filter(|(_, value)| value.is_some())
                            .map(|(index, _)| index.to_string())
                            .collect(),
                    );
                }
                return Some(self.scalar_key(&parameter.name).into_iter().collect());
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
                let index = parameter.index.as_deref()?;
                if index != "@" && index != "*" {
                    return None;
                }
                let values = if let Some(assoc) = self.assoc.get(&parameter.name) {
                    assoc.values().cloned().collect::<Vec<_>>()
                } else {
                    self.arrays
                        .get(&parameter.name)
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

        let index = parameter.index.as_deref()?;
        if index != "@" && index != "*" {
            return None;
        }

        if parameter.operator == Some(ParamOp::Substring) {
            let argument = parameter.argument.as_deref().unwrap_or("");
            let Some((offset, length)) = self.slice_parts(argument) else {
                return Some(Vec::new());
            };
            if let Some(array) = self.arrays.get(&parameter.name) {
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
                .get(&parameter.name)
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
                .get(&parameter.name)
                .map(|values| values.iter().flatten().cloned().collect())
                .or_else(|| {
                    self.assoc
                        .get(&parameter.name)
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
            Err(message) => {
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
        use std::os::fd::{AsRawFd, IntoRawFd};

        let Ok((read, write)) = pipe2(OFlag::empty()) else {
            return String::new();
        };

        // SAFETY: the child only dup2s pipe ends, runs shell code and exits.
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                reset_child_signals();
                self.interactive = false;
                self.job_pgid = None;
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
                let status = match self.eval(source) {
                    Ok(code) => code,
                    Err(err) => {
                        eprintln!("sharky: {}", err.message);
                        2
                    }
                };
                unsafe { _exit(status) }
            }
            Ok(ForkResult::Parent { child }) => {
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
            Err(_) => String::new(),
        }
    }

    fn fifo_substitution(&mut self, source: &str, input: bool) -> String {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;

        let mut attempt = 0;
        let path = loop {
            if attempt >= 8 {
                return String::new();
            }
            let path = std::env::temp_dir().join(format!(
                "sharky-psub-{}-{attempt}",
                std::process::id()
            ));
            attempt += 1;
            match mkfifo(&path, Mode::from_bits_truncate(0o600)) {
                Ok(()) => break path,
                Err(nix::errno::Errno::EEXIST) => continue,
                Err(_) => return String::new(),
            }
        };
        let path_text = path.to_string_lossy().into_owned();
        let Ok((ready_read, ready_write)) = pipe2(OFlag::O_CLOEXEC) else {
            let _ = std::fs::remove_file(&path);
            return String::new();
        };

        // SAFETY: the child opens the FIFO, runs shell code and exits.
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                reset_child_signals();
                self.interactive = false;
                self.job_pgid = None;
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
                let status = match self.eval(source) {
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
                drop(ready_write);
                self.substitution_pids.push(child);
                self.substitution_fds.push(-1);
                self.substitution_paths.push(path_text.clone());
                self.substitution_ready.push(ready_read.into_raw_fd());
                path_text
            }
            Err(_) => {
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
                    let mut byte = [0u8; 1];
                    // SAFETY: the descriptor is the read end of the FIFO
                    // readiness pipe created for this substitution.
                    unsafe {
                        let _ = nix::libc::read(
                            ready,
                            byte.as_mut_ptr() as *mut _,
                            byte.len(),
                        );
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
        use std::io::Read;
        use std::os::fd::{FromRawFd, IntoRawFd};

        let Ok((read, write)) = pipe2(OFlag::O_CLOEXEC) else {
            return String::new();
        };

        // SAFETY: the child runs shell code and exits without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                drop(write);
                let mut file = unsafe { std::fs::File::from_raw_fd(read.into_raw_fd()) };
                let mut output = String::new();
                let _ = file.read_to_string(&mut output);
                let status = exec::wait(child);
                self.substitution_status = Some(status);
                while output.ends_with('\n') {
                    output.pop();
                }
                output
            }
            Ok(ForkResult::Child) => {
                reset_child_signals();
                self.interactive = false;
                self.job_pgid = None;
                let _ = move_fd(write.as_raw_fd(), 1);
                unsafe {
                    nix::libc::close(read.as_raw_fd());
                    nix::libc::close(write.as_raw_fd());
                }
                let code = match parser::parse(source) {
                    Ok(program) => flow_status(self.run_program(&program)),
                    Err(err) => {
                        eprintln!("sharky: {}", err.message);
                        2
                    }
                };
                unsafe { _exit(code) }
            }
            Err(_) => String::new(),
        }
    }

    fn glob_fields(&self, fields: Vec<(String, bool)>) -> Vec<String> {
        let mut out = Vec::new();

        for (text, eligible) in fields {
            if eligible && has_glob(&text) {
                let matches = glob(&text, self.globstar);
                if !matches.is_empty() {
                    out.extend(matches);
                    continue;
                }
            }
            out.push(strip_quoted_glob_markers(&text));
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
    open(path, flags, Mode::empty()).ok()
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
    open(Path::new(path), OFlag::O_RDWR | OFlag::O_NONBLOCK, Mode::empty()).ok()
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

fn describe_and_or(and_or: &AndOr) -> String {
    let mut text = describe_pipeline(&and_or.first);
    for (is_and, pipeline) in &and_or.rest {
        text.push_str(if *is_and { " && " } else { " || " });
        text.push_str(&describe_pipeline(pipeline));
    }
    text
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
        Command::Function { name, body } => {
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
        RedirectOp::Append => format!("{}>> {target}", fd(redirect.fd, 1)),
        RedirectOp::In => format!("{}< {target}", fd(redirect.fd, 0)),
        RedirectOp::Dup(other) => format!("{}>&{other}", fd(redirect.fd, 1)),
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
    }
}

fn flow_status(flow: Flow) -> i32 {
    match flow {
        Flow::Status(code) | Flow::Exit(code) | Flow::Discard(code) | Flow::Return(code) => code,
        Flow::Break | Flow::Continue => 0,
    }
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
                self.set_var(name, value);
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

fn is_builtin_name(name: &str) -> bool {
    matches!(
        name,
        ":" | "true"
            | "false"
            | "cd"
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
            | "exec"
            | "time"
            | "help"
            | "echo"
            | "printf"
    )
}

fn is_declaration_word(name: &str) -> bool {
    matches!(name, "declare" | "local" | "readonly" | "typeset")
}

fn is_special_name(name: &str) -> bool {
    matches!(name, "?" | "#" | "@" | "*" | "!" | "$" | "-")
        || (!name.is_empty() && name.chars().all(|char| char.is_ascii_digit()))
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

fn quote_value(value: &str) -> String {
    if value.chars().any(char::is_control) {
        ansi_quote(value)
    } else {
        double_quote(value)
    }
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

fn normalize_signal(name: &str) -> Option<&'static str> {
    let upper = name.to_ascii_uppercase();
    let signal = upper.strip_prefix("SIG").unwrap_or(&upper);
    if signal == "0" || signal == "EXIT" {
        return Some("EXIT");
    }
    SIGNALS.iter().copied().find(|candidate| *candidate == signal)
}

fn read_stdin_line() -> Option<String> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        // SAFETY: fd 0 is the shell's stdin and the buffer is one byte.
        let count = unsafe { nix::libc::read(0, byte.as_mut_ptr() as *mut _, 1) };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
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
        None
    } else {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
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

fn temp_file(text: &str) -> Result<std::fs::File, String> {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        "sharky-here-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(name);

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|err| format!("heredoc: {err}"))?;
    file.write_all(text.as_bytes())
        .map_err(|err| format!("heredoc: {err}"))?;
    let _ = std::fs::remove_file(&path);
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

// shortcut: allocates in the forked child; safe while sharky is single-threaded.
fn apply_actions(actions: &[Action]) -> Result<(), String> {
    for action in actions {
        match action {
            Action::Write(fd, path) => {
                let file = std::fs::File::create(path)
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
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == QUOTED_GLOB_MARKER {
            index += 2;
            continue;
        }
        if matches!(chars[index], '*' | '?' | '[') {
            return true;
        }
        if matches!(chars[index], '@' | '+' | '!') && chars.get(index + 1) == Some(&'(') {
            return true;
        }
        index += 1;
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

fn glob(pattern: &str, globstar: bool) -> Vec<String> {
    if globstar && pattern_has_globstar(pattern) {
        return globstar_glob(pattern);
    }
    let (dir, base) = match pattern.rfind('/') {
        Some(index) => (&pattern[..index], &pattern[index + 1..]),
        None => (".", pattern),
    };
    if base.is_empty() {
        return Vec::new();
    }

    let directory = if dir.is_empty() { "/" } else { dir };
    let directories = if has_glob(directory) {
        glob(directory, globstar)
    } else {
        vec![directory.to_string()]
    };

    let mut matches = Vec::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !base.starts_with('.') {
                continue;
            }
            if !glob_match(base, &name) {
                continue;
            }
            matches.push(match pattern.rfind('/') {
                Some(_) => format!("{}/{name}", directory.trim_end_matches('/')),
                None => name,
            });
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

fn gs_walk(
    prefix: &str,
    components: &[&str],
    descend_at: &[bool],
    index: usize,
    trailing: bool,
    bare_run_multiple: bool,
    literal_prefix: bool,
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
            gs_entries(prefix, trailing, stats, out);
            return;
        }
        let descend = descend_at[index];
        gs_walk(
            prefix,
            components,
            descend_at,
            index + 1,
            trailing,
            bare_run_multiple,
            literal_prefix,
            stats,
            out,
        );
        // Bash 5.3.20: a symlinked dir is followed only for the next component, not for deeper ** levels.
        let Ok(entries) = std::fs::read_dir(gs_dir(prefix)) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !gs_is_dir(&entry, descend) {
                continue;
            }
            let resume = if gs_is_dir(&entry, false) { index } else { index + 1 };
            gs_walk(
                &gs_join(prefix, &name),
                components,
                descend_at,
                resume,
                trailing,
                bare_run_multiple,
                literal_prefix,
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
        if name.starts_with('.') && !component.starts_with('.') {
            continue;
        }
        if !glob_match(component, &name) {
            continue;
        }
        gs_walk(
            &gs_join(prefix, &name),
            components,
            descend_at,
            index + 1,
            trailing,
            bare_run_multiple,
            literal_prefix,
            stats,
            out,
        );
    }
}

fn gs_entries(prefix: &str, trailing: bool, stats: &mut GlobStats, out: &mut BTreeSet<String>) {
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
        if name.starts_with('.') {
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
            gs_entries(&path, trailing, stats, out);
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

fn globstar_glob(pattern: &str) -> Vec<String> {
    globstar_glob_traced(pattern, &mut GlobStats::default())
}

fn globstar_glob_traced(pattern: &str, stats: &mut GlobStats) -> Vec<String> {
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
    gs_walk(
        &prefix,
        &components,
        &descend_at,
        0,
        trailing,
        bare_run_multiple,
        literal_prefix,
        stats,
        &mut matches,
    );
    matches.into_iter().collect()
}

fn trim_prefix(value: &str, pattern: &str, longest: bool) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut indices: Vec<usize> = (0..=chars.len()).collect();
    if longest {
        indices.reverse();
    }

    for index in indices {
        let prefix: String = chars[..index].iter().collect();
        if glob_match(pattern, &prefix) {
            return chars[index..].iter().collect();
        }
    }

    value.to_string()
}

fn trim_suffix(value: &str, pattern: &str, longest: bool) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut indices: Vec<usize> = (0..=chars.len()).collect();
    if !longest {
        indices.reverse();
    }

    for index in indices {
        let suffix: String = chars[index..].iter().collect();
        if glob_match(pattern, &suffix) {
            return chars[..index].iter().collect();
        }
    }

    value.to_string()
}

fn brace_expand(text: &str) -> Option<Vec<String>> {
    let open = text.find('{')?;
    let mut depth = 0usize;
    let mut close = None;

    for (offset, char) in text[open..].char_indices() {
        match char {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = Some(open + offset);
                    break;
                }
            }
            _ => {}
        }
    }

    let close = close?;
    let prefix = &text[..open];
    let inner = &text[open + 1..close];
    let suffix = &text[close + 1..];
    let parts = split_top_level(inner);

    let alternatives: Vec<String> = if parts.len() > 1 {
        parts
            .iter()
            .map(|part| format!("{prefix}{part}{suffix}"))
            .collect()
    } else {
        range_values(inner)?
            .iter()
            .map(|value| format!("{prefix}{value}{suffix}"))
            .collect()
    };

    let mut out = Vec::new();
    for alternative in alternatives {
        match brace_expand(&alternative) {
            Some(nested) => out.extend(nested),
            None => out.push(alternative),
        }
        if out.len() > 1024 {
            return None;
        }
    }

    Some(out)
}

fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;

    for (index, char) in text.char_indices() {
        match char {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
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
            values.push(value.to_string());
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

pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_chars(&pattern, &text)
}

fn glob_chars(pattern: &[char], text: &[char]) -> bool {
    if let Some((operator, alternatives, rest)) = split_extglob(pattern) {
        for consumed in 0..=text.len() {
            if ext_matches(operator, &alternatives, &text[..consumed])
                && glob_chars(rest, &text[consumed..])
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
            !text.is_empty() && literal == text[0] && glob_chars(&pattern[2..], &text[1..])
        }
        '*' => {
            let mut index = 0;
            loop {
                if glob_chars(&pattern[1..], &text[index..]) {
                    return true;
                }
                if index == text.len() {
                    return false;
                }
                index += 1;
            }
        }
        '?' => !text.is_empty() && glob_chars(&pattern[1..], &text[1..]),
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
                if index + 2 < pattern.len() && pattern[index + 1] == '-' && pattern[index + 2] != ']'
                {
                    if pattern[index] <= value && value <= pattern[index + 2] {
                        matched = true;
                    }
                    index += 3;
                } else {
                    if pattern[index] == value {
                        matched = true;
                    }
                    index += 1;
                }
            }

            if index >= pattern.len() {
                return false;
            }
            matched != negate && glob_chars(&pattern[index + 1..], &text[1..])
        }
        char => !text.is_empty() && char == text[0] && glob_chars(&pattern[1..], &text[1..]),
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
}
