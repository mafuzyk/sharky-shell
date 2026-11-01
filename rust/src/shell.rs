use std::collections::{BTreeMap, HashMap};
use std::os::fd::{AsRawFd, RawFd};
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
    AndOr, Assignment, AssignmentValue, CaseArm, CaseTerm, Command, List, Pipeline, Program,
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
    arrays: HashMap<String, Vec<String>>,
    assoc: HashMap<String, BTreeMap<String, String>>,
    positional: Vec<String>,
    aliases: HashMap<String, String>,
    abbreviations: HashMap<String, String>,
    wake: Vec<String>,
    keybinds: HashMap<String, String>,
    settings: HashMap<String, String>,
    script: Option<String>,
    interactive: bool,
    errexit: bool,
    nounset: bool,
    xtrace: bool,
    pipefail: bool,
    errexit_suppressed: bool,
    nounset_failed: bool,
    traps: HashMap<String, String>,
    exit_trap_ran: bool,
    history: Option<History>,
    histexpand: bool,
    vi: bool,
    z: Option<Z>,
    config_dir: Option<PathBuf>,
    substitution_fds: Vec<i32>,
    substitution_pids: Vec<Pid>,
    pub exit_requested: Option<i32>,
    jobs: Vec<Job>,
    next_job: usize,
    job_pgid: Option<Pid>,
    last_bg_pid: Option<Pid>,
    colors: HashMap<String, String>,
    styles: HashMap<String, String>,
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
    Break,
    Continue,
    Return(i32),
}

struct Prepared {
    argv: Vec<String>,
    actions: Vec<Action>,
    env: Vec<(String, String)>,
}

enum Action {
    Write(i32, PathBuf),
    Append(i32, PathBuf),
    Read(i32, PathBuf),
    Dup { from: i32, to: i32 },
    Here { fd: i32, file: std::fs::File },
}

impl Shell {
    pub fn new() -> Self {
        let env = std::env::vars_os()
            .filter_map(|(key, value)| {
                Some((
                    key.into_string().ok()?,
                    String::from_utf8(value.into_vec()).ok()?,
                ))
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
            interactive: false,
            errexit: false,
            nounset: false,
            xtrace: false,
            pipefail: false,
            errexit_suppressed: false,
            nounset_failed: false,
            traps: HashMap::new(),
            exit_trap_ran: false,
            history: None,
            histexpand: true,
            vi: false,
            z: None,
            config_dir: None,
            substitution_fds: Vec::new(),
            substitution_pids: Vec::new(),
            exit_requested: None,
            jobs: Vec::new(),
            next_job: 1,
            job_pgid: None,
            last_bg_pid: None,
            colors: HashMap::new(),
            styles: HashMap::new(),
        };
        shell.set_var("PWD", cwd.display().to_string());
        shell
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
        self.finish(status)
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
            Flow::Status(code) | Flow::Exit(code) | Flow::Return(code) => code,
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
            "break", "continue", "return", "jobs", "fg", "bg", "declare",
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

    fn reclaim_terminal(&self) {
        if self.foreground_control() {
            // SAFETY: fd 0 is the terminal for the whole life of an interactive shell.
            let _ = tcsetpgrp(unsafe { std::os::fd::BorrowedFd::borrow_raw(0) }, getpgrp());
        }
    }

    fn run_program(&mut self, program: &Program) -> Flow {
        let mut status = self.last_status;

        for line in &program.lines {
            match self.run_list(line) {
                Flow::Status(code) => {
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
                self.errexit_suppressed = index + 1 != and_or.rest.len();
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
            self.nounset_failed = false;
            self.exit_requested = Some(1);
            return Flow::Exit(1);
        }
        match flow {
            Flow::Status(code)
                if code != 0 && self.errexit && !self.errexit_suppressed =>
            {
                self.exit_requested = Some(code);
                Flow::Exit(code)
            }
            other => other,
        }
    }

    fn run_pipeline(&mut self, pipeline: &Pipeline) -> Flow {
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
                self.cleanup_substitutions();
                return flow;
            }
            let flow = self.run_command(&pipeline.commands[0]);
            let flow = self.errexit_check(flow);
            self.cleanup_substitutions();
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
        for (index, command) in pipeline.commands.iter().enumerate() {
            let stdin = index.checked_sub(1).map(|i| pipes[i].0.as_raw_fd());
            let stdout = pipes.get(index).map(|pair| pair.1.as_raw_fd());

            // SAFETY: the child runs shell code and exits without unwinding.
            match unsafe { fork() } {
                Ok(ForkResult::Parent { child }) => {
                    if self.interactive {
                        if leader.is_none() {
                            if self.foreground_control() {
                                // SAFETY: fd 0 is the terminal; this child leads the job.
                                let _ = tcsetpgrp(
                                    unsafe { std::os::fd::BorrowedFd::borrow_raw(0) },
                                    child,
                                );
                            }
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
        self.cleanup_substitutions();
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
            Command::Test(words) => Flow::Status(self.run_test(words)),
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

    fn run_test(&mut self, words: &[Word]) -> i32 {
        let mut expand = |word: &Word| self.expand_text_word(word);
        match test_expr::evaluate(words, &mut expand) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(message) => {
                eprintln!("sharky: [[: {message}");
                2
            }
        }
    }

    fn run_simple(&mut self, simple: &Simple) -> Flow {
        if simple.words.is_empty() && simple.redirects.is_empty() {
            for assignment in &simple.assignments {
                self.assign(assignment);
            }
            if self.nounset_failed {
                self.nounset_failed = false;
                self.exit_requested = Some(1);
                return Flow::Exit(1);
            }
            return Flow::Status(0);
        }

        let mut prepared = match self.prepare(simple) {
            Ok(prepared) => prepared,
            Err(message) => {
                eprintln!("sharky: {message}");
                return Flow::Status(1);
            }
        };

        if self.nounset_failed {
            self.nounset_failed = false;
            self.exit_requested = Some(1);
            return Flow::Exit(1);
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

        if prepared.actions.is_empty() {
            if prepared.argv.is_empty() {
                return Flow::Status(0);
            }

            if let Some(body) = self.funcs.get(&prepared.argv[0]).cloned() {
                let saved = std::mem::replace(&mut self.positional, prepared.argv[1..].to_vec());
                let flow = self.run_list(&body);
                self.positional = saved;
                return match flow {
                    Flow::Return(code) => Flow::Status(code),
                    other => other,
                };
            }

            if let Some(flow) = self.builtin(&prepared.argv) {
                return flow;
            }
        }

        let command = prepared.argv.join(" ");
        match self.spawn(
            &prepared.argv,
            &prepared.actions,
            &prepared.env,
            None,
            None,
            None,
        ) {
            Ok(pid) => Flow::Status(self.wait_foreground(&[pid], pid, command)),
            Err(message) => {
                eprintln!("sharky: {message}");
                Flow::Status(1)
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
        if simple.words.is_empty() && simple.redirects.is_empty() {
            for assignment in &simple.assignments {
                self.assign(assignment);
            }
            return i32::from(self.nounset_failed);
        }

        let prepared = match self.prepare(simple) {
            Ok(prepared) => prepared,
            Err(message) => {
                eprintln!("sharky: {message}");
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
            let flow = self.run_list(&body);
            self.positional = saved;
            return match flow {
                Flow::Return(code) => code,
                other => flow_status(other),
            };
        }

        if let Some(flow) = self.builtin(&prepared.argv) {
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

        match exec::exec_now(&path, &argv, env) {
            Ok(()) => Flow::Status(0),
            Err(err) => {
                eprintln!("sharky: exec: {program}: {err}");
                self.exec_failure(126)
            }
        }
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
            if let Flow::Status(0) = self.run_suppressed(condition) {
                return self.run_list(body);
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
                patterns.iter().any(|pattern| {
                    let pattern = self.expand_text_word(pattern);
                    glob_match(&pattern, &value)
                })
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

        if self.xtrace && !argv.is_empty() {
            eprintln!("+ {}", argv.join(" "));
        }

        let actions = self.redirect_actions(&simple.redirects)?;

        let mut env = self.env.clone();
        for assignment in &simple.assignments {
            match &assignment.value {
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

        Ok(Prepared {
            argv,
            actions,
            env,
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

        Ok(actions)
    }

    fn spawn(
        &mut self,
        argv: &[String],
        actions: &[Action],
        env: &[(String, String)],
        stdin: Option<RawFd>,
        stdout: Option<RawFd>,
        leader: Option<Pid>,
    ) -> Result<Pid, String> {
        // SAFETY: the child only runs syscalls and allocation-safe code before exec.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                if self.foreground_control() && leader.is_none() {
                    // SAFETY: fd 0 is the terminal; the child was just placed in its own group.
                    let _ = tcsetpgrp(
                        unsafe { std::os::fd::BorrowedFd::borrow_raw(0) },
                        child,
                    );
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
                if let Some(flow) = self.builtin(argv) {
                    unsafe { _exit(flow_status(flow)) }
                }
                match exec::resolve(&argv[0], self) {
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

    fn builtin(&mut self, words: &[String]) -> Option<Flow> {
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
            "jobs" => Some(Flow::Status(self.list_jobs())),
            "pwd" => {
                println!("{}", self.cwd.display());
                Some(Flow::Status(0))
            }
            "type" => Some(Flow::Status(self.type_builtin(&words[1..]))),
            "alias" => Some(Flow::Status(self.alias(&words[1..]))),
            "abbr" => Some(Flow::Status(self.abbr(&words[1..]))),
            "unabbr" => Some(Flow::Status(self.unabbr(&words[1..]))),
            "unalias" => Some(Flow::Status(self.unalias(&words[1..]))),
            "history" => Some(Flow::Status(self.history_builtin(&words[1..]))),
            "wait" => Some(Flow::Status(self.wait_builtin(&words[1..]))),
            "kill" => Some(Flow::Status(self.kill_builtin(&words[1..]))),
            "declare" => Some(Flow::Status(self.declare(&words[1..]))),
            "z" => Some(Flow::Status(self.z_builtin(&words[1..], false))),
            "zi" => Some(Flow::Status(self.z_builtin(&words[1..], true))),
            "reload" => Some(Flow::Status(self.reload())),
            "trap" => Some(Flow::Status(self.trap(&words[1..]))),
            "read" => Some(Flow::Status(self.read(&words[1..]))),
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
            "@" | "*" => return self.positional.join(" "),
            _ => {}
        }

        if let Some(values) = self.arrays.get(name) {
            return values.first().cloned().unwrap_or_default();
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
                let mut values = Vec::new();
                for element in elements {
                    values.extend(self.expand_word(element));
                }
                if self.assoc.contains_key(&assignment.name) {
                    let mut map = BTreeMap::new();
                    for value in values {
                        if let Some((key, value)) = assoc_literal(&value) {
                            map.insert(key, value);
                        }
                    }
                    if assignment.append {
                        self.assoc
                            .entry(assignment.name.clone())
                            .or_default()
                            .extend(map);
                    } else {
                        self.assoc.insert(assignment.name.clone(), map);
                    }
                } else {
                    self.set_array(&assignment.name, values, assignment.append);
                }
            }
            AssignmentValue::Word(word) => {
                let value = self.expand_assignment(word);
                if self.assoc.contains_key(&assignment.name) {
                    let key = assignment.index.clone().unwrap_or_else(|| "0".to_string());
                    self.assoc
                        .entry(assignment.name.clone())
                        .or_default()
                        .insert(key, value);
                } else {
                    match &assignment.index {
                        Some(index) => self.set_array_element(&assignment.name, index, value),
                        None => self.set_var(&assignment.name, value),
                    }
                }
            }
        }
    }

    fn declare(&mut self, args: &[String]) -> i32 {
        let mut kind = None;
        let mut names = Vec::new();
        for arg in args {
            match arg.as_str() {
                "-A" => kind = Some(true),
                "-a" => kind = Some(false),
                other if other.starts_with('-') => {
                    eprintln!("sharky: declare: unsupported option: {other}");
                    return 2;
                }
                other => names.push(other.to_string()),
            }
        }

        let Some(assoc) = kind else {
            eprintln!("sharky: declare: usage: declare -A|-a name ...");
            return 2;
        };
        if names.is_empty() {
            eprintln!("sharky: declare: usage: declare -A|-a name ...");
            return 2;
        }

        for name in names {
            if assoc {
                let values = self.arrays.remove(&name).unwrap_or_default();
                let map = self.assoc.entry(name).or_default();
                for value in values {
                    if let Some((key, value)) = assoc_literal(&value) {
                        map.insert(key, value);
                    }
                }
            } else {
                self.assoc.remove(&name);
                self.arrays.entry(name).or_default();
            }
        }
        0
    }

    fn set_array(&mut self, name: &str, values: Vec<String>, append: bool) {
        if append {
            self.arrays.entry(name.to_string()).or_default().extend(values);
        } else {
            self.arrays.insert(name.to_string(), values);
        }
    }

    fn resolve_array_index(&mut self, text: &str) -> usize {
        if let Ok(index) = text.trim().parse::<usize>() {
            return index;
        }
        arith::evaluate(text, self).unwrap_or(0).max(0) as usize
    }

    fn set_array_element(&mut self, name: &str, index: &str, value: String) {
        let index = self.resolve_array_index(index);
        let array = self.arrays.entry(name.to_string()).or_default();
        while array.len() <= index {
            array.push(String::new());
        }
        array[index] = value;
    }

    pub(crate) fn set_var(&mut self, name: &str, value: String) {
        match self.env.iter_mut().find(|(key, _)| key == name) {
            Some(entry) => entry.1 = value,
            None => self.env.push((name.to_string(), value)),
        }
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
                if let Some(assoc) = self.assoc.get_mut(&array) {
                    assoc.remove(&index);
                    continue;
                }
                if let Some(values) = self.arrays.get_mut(&array) {
                    if let Ok(index) = index.parse::<usize>() {
                        if index < values.len() {
                            values.remove(index);
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

        if self.foreground_control() {
            // SAFETY: fd 0 is the terminal for the life of an interactive shell.
            let _ = tcsetpgrp(unsafe { std::os::fd::BorrowedFd::borrow_raw(0) }, job.pgid);
        }
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
        flow
    }

    pub fn init_history(&mut self) {
        self.history = Some(History::load(History::default_path(), self.history_limit()));
    }

    pub fn push_history(&mut self, line: &str) {
        if let Some(history) = &mut self.history {
            history.add(line);
        }
    }

    pub fn save_history(&self) {
        if let Some(history) = &self.history {
            history.save();
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
            if let Some(history) = &mut self.history {
                history.clear();
            }
            return 0;
        }

        eprintln!("sharky: history: usage: history [-c]");
        1
    }

    fn type_builtin(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            eprintln!("sharky: type: usage: type name ...");
            return 1;
        }

        let mut status = 0;
        for name in args {
            if let Some(value) = self.aliases.get(name) {
                println!("{name}: aliased to {value}");
            } else if self.funcs.contains_key(name) {
                println!("{name}: function");
            } else if is_builtin_name(name) {
                println!("{name}: shell builtin");
            } else if let Some(path) = exec::resolve(name, self) {
                println!("{name}: {}", path.display());
            } else {
                eprintln!("sharky: type: {name}: not found");
                status = 1;
            }
        }
        status
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
        let names = if names.is_empty() {
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
        let fields = split_read_fields(&line, !raw, names.len());
        for (name, value) in names.iter().zip(fields) {
            self.set_var(name, value);
        }
        if ended { 0 } else { 1 }
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

    fn set(&mut self, args: &[String]) -> i32 {
        if args.first().map(String::as_str) == Some("--") {
            self.positional = args[1..].to_vec();
            return 0;
        }

        if args.is_empty() {
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
            if !active.is_empty() {
                println!("{}", active.join(" "));
            }
            return 0;
        }

        let mut status = 0;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "-e" => self.errexit = true,
                "+e" => self.errexit = false,
                "-H" => self.histexpand = true,
                "+H" => self.histexpand = false,
                "-u" => self.nounset = true,
                "+u" => self.nounset = false,
                "-x" => self.xtrace = true,
                "+x" => self.xtrace = false,
                option @ ("-o" | "+o") => {
                    let on = option.starts_with('-');
                    index += 1;
                    match args.get(index).map(String::as_str) {
                        Some("pipefail") => self.pipefail = on,
                        Some("vi") => self.vi = on,
                        Some(name) => {
                            eprintln!("sharky: set: {name}: invalid option name");
                            status = 1;
                        }
                        None => {
                            eprintln!("sharky: set: {option}: option name required");
                            status = 1;
                        }
                    }
                }
                name => {
                    if let Some((key, value)) = name.split_once('=') {
                        self.set_var(key, value.to_string());
                    } else {
                        eprintln!("sharky: set: {name}: invalid option");
                        status = 1;
                    }
                }
            }
            index += 1;
        }
        status
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

        let mut fields = Fields::new();

        for part in &word.parts {
            match part {
                Part::Literal(text) => fields.push(text, false),
                Part::Single(text) | Part::Escaped(text) => fields.push(text, true),
                Part::Tilde => {
                    let home = self.var("HOME");
                    fields.push(&home, true);
                }
                Part::Double(inner) => {
                    if let [Part::Parameter(parameter)] = inner.as_slice()
                        && let Some(values) = self.parameter_elements(parameter)
                    {
                        if parameter.index.as_deref() == Some("*") {
                            fields.field_quoted(&values.join(" "));
                        } else {
                            for value in values {
                                fields.field_quoted(&value);
                            }
                        }
                    } else {
                        let mut text = String::new();
                        self.expand_parts(inner, &mut text);
                        fields.push(&text, true);
                    }
                }
                Part::Variable(name) if name == "@" || name == "*" => {
                    for value in &self.positional {
                        fields.field(value);
                    }
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
                        if parameter.index.as_deref() == Some("*") {
                            fields.push(&values.join(" "), false);
                        } else {
                            for value in values {
                                fields.field(&value);
                            }
                        }
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
                        fields.push("0", false);
                    }
                },
            }
        }

        self.glob_fields(fields.finish())
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
        for part in parts {
            match part {
                Part::Literal(text) | Part::Single(text) | Part::Escaped(text) => out.push_str(text),
                Part::Double(inner) => self.expand_parts(inner, out),
                Part::Variable(name) => {
                    let value = self.expansion_var(name);
                    out.push_str(&value);
                }
                Part::Parameter(parameter) => out.push_str(&self.parameter_value(parameter)),
                Part::ProcessSub(source, input) => {
                    out.push_str(&self.process_substitution(source, *input))
                }
                Part::CmdSub(source) => out.push_str(&self.command_substitution(source)),
                Part::Arithmetic(source) => match arith::evaluate(source, self) {
                    Ok(value) => out.push_str(&value.to_string()),
                    Err(message) => eprintln!("sharky: arithmetic: {message}"),
                },
                Part::Tilde => out.push_str(&self.var("HOME")),
            }
        }
    }

    fn expand_string(&mut self, text: &str) -> String {
        let Ok(tokens) = lexer::tokenize(text) else {
            return text.to_string();
        };

        let mut out = String::new();
        for token in tokens {
            if let TokenKind::Word(word) = token.kind {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&self.expand_text_word(&word));
            }
        }
        out
    }

    fn parameter_value(&mut self, parameter: &Parameter) -> String {
        let index = parameter.index.as_deref();
        let multiple = matches!(index, Some("@") | Some("*"));

        if parameter.operator == Some(ParamOp::Length) && multiple {
            if let Some(array) = self.arrays.get(&parameter.name) {
                return array.len().to_string();
            }
            if let Some(assoc) = self.assoc.get(&parameter.name) {
                return assoc.len().to_string();
            }
            return "0".to_string();
        }

        if parameter.operator == Some(ParamOp::Keys) && multiple {
            if let Some(array) = self.arrays.get(&parameter.name) {
                return (0..array.len())
                    .map(|index| index.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            if let Some(assoc) = self.assoc.get(&parameter.name) {
                return assoc.keys().cloned().collect::<Vec<_>>().join(" ");
            }
            return String::new();
        }

        let numeric_index = if self.assoc.contains_key(&parameter.name) {
            0
        } else {
            index
                .map(|text| self.resolve_array_index(text))
                .unwrap_or(0)
        };

        let no_nounset = matches!(
            parameter.operator,
            Some(ParamOp::Default | ParamOp::Assign | ParamOp::Alternate | ParamOp::Error)
        );
        let value = if let Some(assoc) = self.assoc.get(&parameter.name) {
            match index {
                Some("@") | Some("*") => assoc.values().cloned().collect::<Vec<_>>().join(" "),
                Some(key) => assoc.get(key).cloned().unwrap_or_default(),
                None => assoc.get("0").cloned().unwrap_or_default(),
            }
        } else if let Some(array) = self.arrays.get(&parameter.name) {
            match index {
                Some("@") | Some("*") => array.join(" "),
                Some(_) => array.get(numeric_index).cloned().unwrap_or_default(),
                None => array.first().cloned().unwrap_or_default(),
            }
        } else if no_nounset {
            self.var(&parameter.name)
        } else {
            self.expansion_var(&parameter.name)
        };
        let argument = parameter.argument.clone().unwrap_or_default();

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
            Some(ParamOp::Assign) => {
                if value.is_empty() {
                    let new = self.expand_string(&argument);
                    self.set_var(&parameter.name, new.clone());
                    new
                } else {
                    value
                }
            }
            Some(ParamOp::Alternate) => {
                if value.is_empty() {
                    String::new()
                } else {
                    self.expand_string(&argument)
                }
            }
            Some(ParamOp::Error) => {
                if value.is_empty() {
                    eprintln!("sharky: {}: {}", parameter.name, argument);
                }
                value
            }
            Some(ParamOp::TrimPrefixShort) => trim_prefix(&value, &argument, false),
            Some(ParamOp::TrimPrefixLong) => trim_prefix(&value, &argument, true),
            Some(ParamOp::TrimSuffixShort) => trim_suffix(&value, &argument, false),
            Some(ParamOp::TrimSuffixLong) => trim_suffix(&value, &argument, true),
            Some(ParamOp::Substring) => substring_value(&value, &argument),
            Some(ParamOp::UpperFirst) => upper_first(&value),
            Some(ParamOp::UpperAll) => value.to_uppercase(),
            Some(ParamOp::LowerFirst) => lower_first(&value),
            Some(ParamOp::LowerAll) => value.to_lowercase(),
            Some(ParamOp::Indirect) => {
                if value.is_empty() {
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
            Some(ParamOp::Keys) => String::new(),
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
        let index = parameter.index.as_deref()?;
        if index != "@" && index != "*" {
            return None;
        }
        match parameter.operator {
            None | Some(ParamOp::Substring) => {}
            _ => return None,
        }

        let mut values = if let Some(assoc) = self.assoc.get(&parameter.name) {
            assoc.values().cloned().collect::<Vec<_>>()
        } else {
            self.arrays.get(&parameter.name).cloned().unwrap_or_default()
        };

        if parameter.operator == Some(ParamOp::Substring) {
            let argument = parameter.argument.as_deref().unwrap_or("");
            let (start, end) = slice_bounds(values.len(), argument);
            values = values[start..end].to_vec();
        }
        Some(values)
    }

    fn process_substitution(&mut self, source: &str, input: bool) -> String {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let Ok((read, write)) = pipe2(OFlag::empty()) else {
            return String::new();
        };

        // SAFETY: the child only dup2s pipe ends, runs shell code and exits.
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
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
                format!("/dev/fd/{fd}")
            }
            Err(_) => String::new(),
        }
    }

    fn cleanup_substitutions(&mut self) {
        for fd in self.substitution_fds.drain(..) {
            let _ = close(fd);
        }
        self.substitution_pids
            .retain(|pid| matches!(waitpid(*pid, Some(WaitPidFlag::WNOHANG)), Ok(WaitStatus::StillAlive)));
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
                exec::wait(child);
                while output.ends_with('\n') {
                    output.pop();
                }
                output
            }
            Ok(ForkResult::Child) => {
                reset_child_signals();
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
                let matches = glob(&text);
                if !matches.is_empty() {
                    out.extend(matches);
                    continue;
                }
            }
            out.push(text);
        }

        out
    }
}

fn slice_bounds(len: usize, argument: &str) -> (usize, usize) {
    let (offset, length) = match argument.split_once(':') {
        Some((offset, length)) => (offset, Some(length)),
        None => (argument, None),
    };
    let offset: i64 = offset.trim().parse().unwrap_or(0);
    let len = len as i64;
    let start = if offset < 0 {
        (len + offset).max(0)
    } else {
        offset.min(len)
    };
    let end = match length {
        Some(length) => {
            let length: i64 = length.trim().parse().unwrap_or(0);
            if length < 0 {
                (len + length).max(start)
            } else {
                (start + length).min(len)
            }
        }
        None => len,
    };
    (start as usize, end as usize)
}

fn substring_value(value: &str, argument: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let (start, end) = slice_bounds(chars.len(), argument);
    chars[start..end].iter().collect()
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
    for (index, char) in pattern.iter().enumerate().skip(1) {
        match char {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;

    let mut alternatives = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0usize;
    for char in &pattern[2..end] {
        match char {
            '(' => {
                depth += 1;
                current.push(*char);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(*char);
            }
            '|' if depth == 0 => alternatives.push(std::mem::take(&mut current)),
            _ => current.push(*char),
        }
    }
    alternatives.push(current);

    Some((operator, alternatives, &pattern[end + 1..]))
}

fn ext_matches(operator: char, alternatives: &[Vec<char>], text: &[char]) -> bool {
    let exact = |text: &[char]| alternatives.iter().any(|alt| glob_chars(alt, text));
    match operator {
        '?' => text.is_empty() || exact(text),
        '@' => exact(text),
        '*' => text.is_empty() || ext_repeat(alternatives, text),
        '+' => !text.is_empty() && ext_repeat(alternatives, text),
        '!' => !exact(text),
        _ => false,
    }
}

fn ext_repeat(alternatives: &[Vec<char>], text: &[char]) -> bool {
    let mut reachable = vec![false; text.len() + 1];
    reachable[0] = true;
    for end in 1..=text.len() {
        for start in 0..end {
            if reachable[start]
                && alternatives
                    .iter()
                    .any(|alt| glob_chars(alt, &text[start..end]))
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
    }
}

fn flow_status(flow: Flow) -> i32 {
    match flow {
        Flow::Status(code) | Flow::Exit(code) | Flow::Return(code) => code,
        Flow::Break | Flow::Continue => 0,
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
            | "history"
            | "wait"
            | "kill"
            | "z"
            | "zi"
            | "reload"
            | "shift"
            | "declare"
    )
}

fn is_special_name(name: &str) -> bool {
    matches!(name, "?" | "#" | "@" | "*" | "!" | "$" | "-")
        || (!name.is_empty() && name.chars().all(|char| char.is_ascii_digit()))
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

fn split_read_fields(line: &str, escapes: bool, count: usize) -> Vec<String> {
    let mut chars = line.chars().peekable();
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut started = false;

    while let Some(&char) = chars.peek() {
        if char == ' ' || char == '\t' || char == '\n' {
            chars.next();
        } else {
            break;
        }
    }

    while let Some(char) = chars.next() {
        if escapes && char == '\\' {
            if let Some(next) = chars.next() {
                current.push(next);
                started = true;
            }
            continue;
        }

        if (char == ' ' || char == '\t' || char == '\n') && fields.len() + 1 < count {
            if started {
                fields.push(std::mem::take(&mut current));
                started = false;
            }
            while let Some(&next) = chars.peek() {
                if next == ' ' || next == '\t' || next == '\n' {
                    chars.next();
                } else {
                    break;
                }
            }
            continue;
        }

        current.push(char);
        started = true;
    }

    if started || fields.len() < count {
        while current.ends_with([' ', '\t', '\n']) {
            current.pop();
        }
        fields.push(current);
    }
    while fields.len() < count {
        fields.push(String::new());
    }
    fields
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

// shortcut: allocates in the forked child; safe while sharky is single-threaded.
fn apply_actions(actions: &[Action]) -> Result<(), String> {
    for action in actions {
        match action {
            Action::Write(fd, path) => {
                let file = std::fs::File::create(path)
                    .map_err(|err| format!("{}: {err}", path.display()))?;
                move_fd(file.as_raw_fd(), *fd)?;
            }
            Action::Append(fd, path) => {
                let file = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path)
                    .map_err(|err| format!("{}: {err}", path.display()))?;
                move_fd(file.as_raw_fd(), *fd)?;
            }
            Action::Read(fd, path) => {
                let file =
                    std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
                move_fd(file.as_raw_fd(), *fd)?;
            }
            Action::Dup { from, to } => {
                move_fd(*from, *to)?;
            }
            Action::Here { fd, file } => {
                move_fd(file.as_raw_fd(), *fd)?;
            }
        }
    }

    Ok(())
}


struct Fields {
    done: Vec<(String, bool)>,
    current: String,
    started: bool,
    glob: bool,
}

impl Fields {
    fn new() -> Self {
        Fields {
            done: Vec::new(),
            current: String::new(),
            started: false,
            glob: false,
        }
    }

    fn push(&mut self, text: &str, quoted: bool) {
        if quoted {
            self.current.push_str(text);
            self.started = true;
            return;
        }

        for char in text.chars() {
            if matches!(char, ' ' | '\t' | '\n') {
                if self.started {
                    self.flush();
                }
            } else {
                self.current.push(char);
                self.started = true;
                self.glob = true;
            }
        }
    }

    fn field_quoted(&mut self, text: &str) {
        if self.started {
            self.flush();
        }
        self.done.push((text.to_string(), false));
    }

    fn field(&mut self, text: &str) {
        if self.started {
            self.flush();
        }
        self.current.push_str(text);
        self.started = true;
        self.glob = true;
        self.flush();
    }

    fn flush(&mut self) {
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
    text.chars().any(|char| matches!(char, '*' | '?' | '['))
        || text.contains("@(")
        || text.contains("+(")
        || text.contains("!(")
}

fn glob(pattern: &str) -> Vec<String> {
    let (dir, base) = match pattern.rfind('/') {
        Some(index) => (&pattern[..index], &pattern[index + 1..]),
        None => (".", pattern),
    };
    if base.is_empty() {
        return Vec::new();
    }

    let directory = if dir.is_empty() { "/" } else { dir };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };

    let mut matches: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !base.starts_with('.') {
                return None;
            }
            if !glob_match(base, &name) {
                return None;
            }
            Some(match pattern.rfind('/') {
                Some(_) => format!("{}/{name}", directory.trim_end_matches('/')),
                None => name,
            })
        })
        .collect();

    matches.sort();
    matches
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
