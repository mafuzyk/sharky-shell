use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use nix::fcntl::OFlag;
use nix::libc::_exit;
use nix::unistd::{ForkResult, Pid, fork, pipe2};

use crate::config::{self, Config};
use crate::ast::{
    AndOr, Command, List, Pipeline, Program, Redirect, RedirectOp, Simple,
};
use crate::exec;
use crate::lexer::{Part, Word};
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
    colors: HashMap<String, String>,
    styles: HashMap<String, String>,
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

        Shell {
            env,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            last_status: 0,
            funcs: HashMap::new(),
            positional: Vec::new(),
            aliases: HashMap::new(),
            keybinds: HashMap::new(),
            settings: HashMap::new(),
            colors: HashMap::new(),
            styles: HashMap::new(),
        }
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

    pub fn run(&mut self, source: &str) -> i32 {
        let program = match parser::parse(source) {
            Ok(program) => program,
            Err(err) => {
                eprintln!("sharky: {}", err.message);
                return 2;
            }
        };

        match self.run_program(&program) {
            Flow::Status(code) | Flow::Exit(code) | Flow::Return(code) => code,
            Flow::Break | Flow::Continue => 0,
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
                eprintln!("sharky: background jobs are not implemented yet");
                return Flow::Status(2);
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
        for (index, command) in pipeline.commands.iter().enumerate() {
            let stdin = index.checked_sub(1).map(|i| pipes[i].0.as_raw_fd());
            let stdout = pipes.get(index).map(|pair| pair.1.as_raw_fd());

            // SAFETY: the child runs shell code and exits without unwinding.
            match unsafe { fork() } {
                Ok(ForkResult::Parent { child }) => pids.push(child),
                Ok(ForkResult::Child) => {
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

        let mut status = 0;
        for pid in pids {
            status = exec::wait(pid);
        }
        Flow::Status(status)
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
        }
    }

    fn run_simple(&mut self, simple: &Simple) -> Flow {
        if simple.words.is_empty() && simple.redirects.is_empty() {
            for assignment in &simple.assignments {
                let value = expand_assignment(&assignment.value, self);
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

        match self.spawn(
            &prepared.argv,
            &prepared.actions,
            &prepared.env,
            None,
            None,
        ) {
            Ok(pid) => Flow::Status(exec::wait(pid)),
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
                let value = expand_assignment(&assignment.value, self);
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
            Ok(ForkResult::Parent { child }) => Flow::Status(exec::wait(child)),
            Ok(ForkResult::Child) => {
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
            Ok(ForkResult::Parent { child }) => Flow::Status(exec::wait(child)),
            Ok(ForkResult::Child) => {
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
            words
                .iter()
                .flat_map(|word| expand_word(word, self))
                .collect()
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
        let value = expand_text_word(word, self);

        for (patterns, body) in arms {
            for pattern in patterns {
                let pattern = expand_text_word(pattern, self);
                if glob_match(&pattern, &value) {
                    return self.run_list(body);
                }
            }
        }

        Flow::Status(0)
    }

    fn prepare(&self, simple: &Simple) -> Result<Prepared, String> {
        let argv: Vec<String> = simple
            .words
            .iter()
            .flat_map(|word| expand_word(word, self))
            .collect();

        let actions = self.redirect_actions(&simple.redirects)?;

        let mut env = self.env.clone();
        for assignment in &simple.assignments {
            let value = expand_assignment(&assignment.value, self);
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
    fn expand_alias(&self, expansion: &str, extra: &[String]) -> Option<Vec<String>> {
        let tokens = crate::lexer::tokenize(expansion).ok()?;
        let mut argv = Vec::new();

        for token in tokens {
            match token.kind {
                crate::lexer::TokenKind::Word(word) => {
                    argv.extend(expand_word(&word, self));
                }
                crate::lexer::TokenKind::Eof => break,
                _ => return None,
            }
        }

        argv.extend_from_slice(extra);
        Some(argv)
    }

    fn redirect_actions(&self, redirects: &[Redirect]) -> Result<Vec<Action>, String> {
        let mut actions = Vec::new();

        for redirect in redirects {
            match redirect.op {
                RedirectOp::Dup(from) => actions.push(Action::Dup {
                    from,
                    to: redirect.fd,
                }),
                op => {
                    let Some(word) = &redirect.target else {
                        return Err("redirect without a target".to_string());
                    };
                    let fields = expand_word(word, self);
                    if fields.len() != 1 {
                        return Err("ambiguous redirect".to_string());
                    }
                    let path = PathBuf::from(&fields[0]);
                    actions.push(match op {
                        RedirectOp::Out => Action::Write(redirect.fd, path),
                        RedirectOp::Append => Action::Append(redirect.fd, path),
                        RedirectOp::In => Action::Read(redirect.fd, path),
                        RedirectOp::Dup(_) => unreachable!(),
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
    ) -> Result<Pid, String> {
        // SAFETY: the child only runs syscalls and allocation-safe code before exec.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => Ok(child),
            Ok(ForkResult::Child) => {
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
            "exit" => Some(Flow::Exit(
                words
                    .get(1)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(self.last_status),
            )),
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
            _ => None,
        }
    }

    pub fn var(&self, name: &str) -> String {
        if !name.is_empty() && name.chars().all(|char| char.is_ascii_digit()) {
            if name == "0" {
                return "sharky".to_string();
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
        let Some(target) = target else {
            eprintln!("sharky: cd: missing operand");
            return 1;
        };

        match std::env::set_current_dir(target) {
            Ok(()) => {
                self.cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
                0
            }
            Err(err) => {
                eprintln!("sharky: cd: {target}: {err}");
                1
            }
        }
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

fn flow_status(flow: Flow) -> i32 {
    match flow {
        Flow::Status(code) | Flow::Exit(code) | Flow::Return(code) => code,
        Flow::Break | Flow::Continue => 0,
    }
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
        }
    }

    Ok(())
}

fn expand_word(word: &Word, shell: &Shell) -> Vec<String> {
    let mut fields = Fields::new();

    for part in &word.parts {
        match part {
            Part::Literal(text) => fields.push(text, false),
            Part::Single(text) | Part::Escaped(text) => fields.push(text, true),
            Part::Double(inner) => {
                let mut text = String::new();
                expand_text(inner, shell, &mut text);
                fields.push(&text, true);
            }
            Part::Variable(name) if name == "@" || name == "*" => {
                for value in &shell.positional {
                    fields.field(value);
                }
            }
            Part::Variable(name) => fields.push(&shell.var(name), false),
        }
    }

    fields.finish()
}

fn expand_text_word(word: &Word, shell: &Shell) -> String {
    let mut text = String::new();
    expand_text(&word.parts, shell, &mut text);
    text
}

fn expand_assignment(word: &Word, shell: &Shell) -> String {
    expand_text_word(word, shell)
}

fn expand_text(parts: &[Part], shell: &Shell, out: &mut String) {
    for part in parts {
        match part {
            Part::Literal(text) | Part::Single(text) | Part::Escaped(text) => out.push_str(text),
            Part::Double(inner) => expand_text(inner, shell, out),
            Part::Variable(name) => out.push_str(&shell.var(name)),
        }
    }
}

struct Fields {
    done: Vec<String>,
    current: String,
    started: bool,
}

impl Fields {
    fn new() -> Self {
        Fields {
            done: Vec::new(),
            current: String::new(),
            started: false,
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
            }
        }
    }

    fn field(&mut self, text: &str) {
        if self.started {
            self.flush();
        }
        self.current.push_str(text);
        self.started = true;
        self.flush();
    }

    fn flush(&mut self) {
        self.done.push(std::mem::take(&mut self.current));
        self.started = false;
    }

    fn finish(mut self) -> Vec<String> {
        if self.started {
            self.flush();
        }
        self.done
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
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
