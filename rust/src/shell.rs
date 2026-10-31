use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use nix::fcntl::OFlag;
use nix::libc::_exit;
use nix::sys::signal::{SigHandler, Signal, killpg, signal};
use nix::sys::wait::WaitStatus;
use nix::unistd::{ForkResult, Pid, fork, getpid, getpgrp, pipe2, setpgid, tcsetpgrp};

use crate::config::{self, Config};
use crate::ast::{
    AndOr, Command, List, Pipeline, Program, Redirect, RedirectOp, Simple, literal,
};
use crate::arith;
use crate::exec;
use crate::test_expr;
use crate::lexer::{self, ParamOp, Parameter, Part, TokenKind, Word};
use crate::parser;

pub struct Shell {
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub last_status: i32,
    funcs: HashMap<String, List>,
    positional: Vec<String>,
    aliases: HashMap<String, String>,
    keybinds: HashMap<String, String>,
    settings: HashMap<String, String>,
    script: Option<String>,
    interactive: bool,
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
            funcs: HashMap::new(),
            positional: Vec::new(),
            aliases: HashMap::new(),
            keybinds: HashMap::new(),
            settings: HashMap::new(),
            script: None,
            interactive: false,
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

        match config::load(&dir) {
            Ok(config) => self.apply_config(config),
            Err(err) => eprintln!("sharky: config error in {err}"),
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
        self.keybinds.extend(config.keybinds);
        self.settings.extend(config.settings);
        self.colors.extend(config.colors);
        self.styles.extend(config.styles);
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
        match self.eval(source) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("sharky: {}", err.message);
                2
            }
        }
    }

    pub fn eval(&mut self, source: &str) -> Result<i32, parser::ParseError> {
        let program = parser::parse(source)?;
        Ok(match self.run_program(&program) {
            Flow::Status(code) | Flow::Exit(code) | Flow::Return(code) => code,
            Flow::Break | Flow::Continue => 0,
        })
    }

    pub fn set_interactive(&mut self, interactive: bool) {
        self.interactive = interactive;
    }

    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(String::as_str)
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
            "break", "continue", "return", "jobs", "fg", "bg",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        words.extend(self.aliases.keys().cloned());
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
        let mut flow = self.run_pipeline(&and_or.first);

        for (is_and, pipeline) in &and_or.rest {
            let status = match &flow {
                Flow::Status(code) => *code,
                _ => return flow,
            };
            let should_run = if *is_and { status == 0 } else { status != 0 };
            if should_run {
                flow = self.run_pipeline(pipeline);
            }
        }

        flow
    }

    fn run_pipeline(&mut self, pipeline: &Pipeline) -> Flow {
        if pipeline.commands.len() == 1 {
            return self.run_command(&pipeline.commands[0]);
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
        Flow::Status(self.wait_foreground(&pids, pgid, command))
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
                let value = self.expand_assignment(&assignment.value);
                self.set_var(&assignment.name, value);
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

        if let Some(name) = prepared.argv.first()
            && let Some(expansion) = self.aliases.get(name).cloned()
            && let Some(argv) = self.expand_alias(&expansion, &prepared.argv[1..])
        {
            prepared.argv = argv;
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
                let value = self.expand_assignment(&assignment.value);
                self.set_var(&assignment.name, value);
            }
            return 0;
        }

        let prepared = match self.prepare(simple) {
            Ok(prepared) => prepared,
            Err(message) => {
                eprintln!("sharky: {message}");
                return 1;
            }
        };

        if let Err(message) = apply_actions(&prepared.actions) {
            eprintln!("sharky: {message}");
            return 1;
        }

        if prepared.argv.is_empty() {
            return 0;
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
            if let Flow::Status(0) = self.run_list(condition) {
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

    fn run_while(&mut self, condition: &List, body: &List, until: bool) -> Flow {
        let mut status = 0;

        loop {
            let condition_status = match self.run_list(condition) {
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

    fn run_case(&mut self, word: &Word, arms: &[(Vec<Word>, List)]) -> Flow {
        let value = self.expand_text_word(word);

        for (patterns, body) in arms {
            for pattern in patterns {
                let pattern = self.expand_text_word(pattern);
                if glob_match(&pattern, &value) {
                    return self.run_list(body);
                }
            }
        }

        Flow::Status(0)
    }

    fn prepare(&mut self, simple: &Simple) -> Result<Prepared, String> {
        let mut argv = Vec::new();
        for word in &simple.words {
            argv.extend(self.expand_word(word));
        }

        let actions = self.redirect_actions(&simple.redirects)?;

        let mut env = self.env.clone();
        for assignment in &simple.assignments {
            let value = self.expand_assignment(&assignment.value);
            match env.iter_mut().find(|(key, _)| key == &assignment.name) {
                Some(entry) => entry.1 = value,
                None => env.push((assignment.name.clone(), value)),
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
                        RedirectOp::Dup(_) | RedirectOp::Here { .. } => unreachable!(),
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
            "fg" => Some(Flow::Status(self.fg(&words[1..]))),
            "bg" => Some(Flow::Status(self.bg(&words[1..]))),
            "source" | "." => Some(self.source(&words[1..])),
            _ => None,
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
            "@" | "*" => return self.positional.join(" "),
            _ => {}
        }

        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    fn set_var(&mut self, name: &str, value: String) {
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
            self.env.retain(|(key, _)| key != name);
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
        let mut status = 0;
        let mut stopped = false;

        for pid in pids {
            match exec::wait_traced(*pid) {
                WaitStatus::Exited(_, code) => status = code,
                WaitStatus::Signaled(_, signal, _) => status = 128 + signal as i32,
                WaitStatus::Stopped(_, _) => {
                    stopped = true;
                    status = 128 + Signal::SIGTSTP as i32;
                }
                _ => {}
            }
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

    fn set(&mut self, args: &[String]) -> i32 {
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "-e" | "-u" | "-x" | "+e" | "+u" | "+x" => {}
                name => {
                    if let Some((key, value)) = name.split_once('=') {
                        self.set_var(key, value.to_string());
                    }
                }
            }
            index += 1;
        }
        0
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
                0
            }
            Err(err) => {
                eprintln!("sharky: cd: {target}: {err}");
                1
            }
        }
    }

    fn expand_word(&mut self, word: &Word) -> Vec<String> {
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
                    let mut text = String::new();
                    self.expand_parts(inner, &mut text);
                    fields.push(&text, true);
                }
                Part::Variable(name) if name == "@" || name == "*" => {
                    for value in &self.positional {
                        fields.field(value);
                    }
                }
                Part::Variable(name) => {
                    let value = self.var(name);
                    fields.push(&value, false);
                }
                Part::Parameter(parameter) => {
                    let value = self.parameter_value(parameter);
                    fields.push(&value, false);
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
                Part::Variable(name) => out.push_str(&self.var(name)),
                Part::Parameter(parameter) => out.push_str(&self.parameter_value(parameter)),
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
        let value = self.var(&parameter.name);
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
            if eligible && text.chars().any(|char| matches!(char, '*' | '?' | '[')) {
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

pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_chars(&pattern, &text)
}

fn glob_chars(pattern: &[char], text: &[char]) -> bool {
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
