use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use nix::fcntl::OFlag;
use nix::libc::_exit;
use nix::unistd::{ForkResult, Pid, fork, pipe2};

use crate::exec;
use crate::lexer::{self, Part, Token, TokenKind, Word};

pub struct Shell {
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub last_status: i32,
}

#[derive(Clone, Copy)]
enum Connector {
    Always,
    And,
    Or,
}

enum Flow {
    Status(i32),
    Exit(i32),
}

#[derive(Default)]
struct SimpleCommand {
    assignments: Vec<(String, Word)>,
    words: Vec<Word>,
    redirects: Vec<Redirect>,
}

#[derive(Clone, Copy)]
enum RedirectKind {
    Write,
    Append,
    Read,
    Dup(i32),
}

struct Redirect {
    fd: i32,
    kind: RedirectKind,
    target: Option<Word>,
}

struct Item {
    pipeline: Vec<SimpleCommand>,
    connector: Connector,
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
        }
    }

    pub fn run(&mut self, source: &str) -> i32 {
        let tokens = match lexer::tokenize(source) {
            Ok(tokens) => tokens,
            Err(err) => {
                eprintln!("sharky: {}", err.message);
                return 2;
            }
        };

        let items = match parse_items(tokens) {
            Ok(items) => items,
            Err(message) => {
                eprintln!("sharky: {message}");
                return 2;
            }
        };

        let mut status = self.last_status;
        let mut connector = Connector::Always;

        for item in &items {
            let should_run = match connector {
                Connector::Always => true,
                Connector::And => status == 0,
                Connector::Or => status != 0,
            };

            if should_run {
                match self.run_item(item) {
                    Flow::Status(code) => {
                        status = code;
                        self.last_status = code;
                    }
                    Flow::Exit(code) => return code,
                }
            }

            connector = item.connector;
        }

        status
    }

    pub fn var(&self, name: &str) -> String {
        if name == "?" {
            return self.last_status.to_string();
        }

        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    fn run_item(&mut self, item: &Item) -> Flow {
        if item.pipeline.len() == 1 {
            self.run_single(&item.pipeline[0])
        } else {
            Flow::Status(self.run_pipeline(&item.pipeline))
        }
    }

    fn run_single(&mut self, command: &SimpleCommand) -> Flow {
        if command.words.is_empty() && command.redirects.is_empty() {
            for (name, word) in &command.assignments {
                let value = expand_assignment(word, self);
                self.set_var(name, value);
            }
            return Flow::Status(0);
        }

        let (argv, actions, env) = match self.prepare(command) {
            Ok(prepared) => prepared,
            Err(message) => {
                eprintln!("sharky: {message}");
                return Flow::Status(1);
            }
        };

        if actions.is_empty() {
            if argv.is_empty() {
                return Flow::Status(0);
            }
            if let Some(flow) = self.builtin(&argv) {
                return flow;
            }
        }

        match self.spawn(&argv, &actions, &env, None, None) {
            Ok(pid) => Flow::Status(exec::wait(pid)),
            Err(message) => {
                eprintln!("sharky: {message}");
                Flow::Status(1)
            }
        }
    }

    fn run_pipeline(&mut self, commands: &[SimpleCommand]) -> i32 {
        let mut prepared = Vec::new();
        for command in commands {
            match self.prepare(command) {
                Ok(item) => prepared.push(item),
                Err(message) => {
                    eprintln!("sharky: {message}");
                    return 1;
                }
            }
        }

        let links = commands.len().saturating_sub(1);
        let mut pipes = Vec::with_capacity(links);
        for _ in 0..links {
            match pipe2(OFlag::O_CLOEXEC) {
                Ok(pair) => pipes.push(pair),
                Err(err) => {
                    eprintln!("sharky: pipe: {err}");
                    return 1;
                }
            }
        }

        let mut pids = Vec::with_capacity(commands.len());
        for (index, (argv, actions, env)) in prepared.iter().enumerate() {
            let stdin = index.checked_sub(1).map(|i| pipes[i].0.as_raw_fd());
            let stdout = pipes.get(index).map(|pair| pair.1.as_raw_fd());
            match self.spawn(argv, actions, env, stdin, stdout) {
                Ok(pid) => pids.push(pid),
                Err(message) => {
                    eprintln!("sharky: {message}");
                    return 1;
                }
            }
        }

        drop(pipes);

        let mut status = 0;
        for pid in pids {
            status = exec::wait(pid);
        }
        status
    }

    fn prepare(
        &self,
        command: &SimpleCommand,
    ) -> Result<(Vec<String>, Vec<Action>, Vec<(String, String)>), String> {
        let argv: Vec<String> = command
            .words
            .iter()
            .flat_map(|word| expand_word(word, self))
            .collect();

        let mut actions = Vec::new();
        for redirect in &command.redirects {
            match redirect.kind {
                RedirectKind::Dup(from) => actions.push(Action::Dup {
                    from,
                    to: redirect.fd,
                }),
                kind => {
                    let Some(word) = &redirect.target else {
                        return Err("redirect without a target".to_string());
                    };
                    let fields = expand_word(word, self);
                    if fields.len() != 1 {
                        return Err("ambiguous redirect".to_string());
                    }
                    let path = PathBuf::from(&fields[0]);
                    actions.push(match kind {
                        RedirectKind::Write => Action::Write(redirect.fd, path),
                        RedirectKind::Append => Action::Append(redirect.fd, path),
                        RedirectKind::Read => Action::Read(redirect.fd, path),
                        RedirectKind::Dup(_) => unreachable!(),
                    });
                }
            }
        }

        let mut env = self.env.clone();
        for (name, word) in &command.assignments {
            let value = expand_assignment(word, self);
            match env.iter_mut().find(|(key, _)| key == name) {
                Some(entry) => entry.1 = value,
                None => env.push((name.clone(), value)),
            }
        }

        Ok((argv, actions, env))
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
                    let code = match flow {
                        Flow::Status(code) | Flow::Exit(code) => code,
                    };
                    unsafe { _exit(code) }
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
            "cd" => Some(Flow::Status(self.cd(words.get(1).map(String::as_str)))),
            "export" => Some(Flow::Status(self.export(&words[1..]))),
            "unset" => Some(Flow::Status(self.unset(&words[1..]))),
            _ => None,
        }
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

fn move_fd(from: RawFd, to: RawFd) -> Result<(), String> {
    if unsafe { nix::libc::dup2(from, to) } < 0 {
        return Err(format!(
            "redirect: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

// shortcut: allocates in the forked child; safe while sharky is single-threaded.
fn apply_actions(actions: &[Action]) -> Result<(), String> {
    for action in actions {
        match action {
            Action::Write(fd, path) => {
                let file =
                    std::fs::File::create(path).map_err(|err| format!("{}: {err}", path.display()))?;
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

fn parse_items(tokens: Vec<Token>) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();
    let mut pipeline = Vec::new();
    let mut command = SimpleCommand::default();
    let mut pending: Option<(i32, RedirectKind)> = None;

    for token in tokens {
        if let Some((fd, kind)) = pending.take() {
            match token.kind {
                TokenKind::Word(word) => {
                    command.redirects.push(Redirect {
                        fd,
                        kind,
                        target: Some(word),
                    });
                    continue;
                }
                _ => return Err("syntax error: redirect without a target".to_string()),
            }
        }

        match token.kind {
            TokenKind::Word(word) => {
                match as_assignment(&word) {
                    Some((name, value)) if command.words.is_empty() => {
                        command.assignments.push((name, value));
                    }
                    _ => command.words.push(word),
                }
            }
            TokenKind::RedirOut(fd) => pending = Some((fd.unwrap_or(1), RedirectKind::Write)),
            TokenKind::RedirAppend(fd) => pending = Some((fd.unwrap_or(1), RedirectKind::Append)),
            TokenKind::RedirIn(fd) => pending = Some((fd.unwrap_or(0), RedirectKind::Read)),
            TokenKind::RedirDup(to, from) => command.redirects.push(Redirect {
                fd: to,
                kind: RedirectKind::Dup(from),
                target: None,
            }),
            TokenKind::Pipe => {
                if command.words.is_empty() && command.redirects.is_empty() {
                    return Err("syntax error near '|'".to_string());
                }
                pipeline.push(std::mem::take(&mut command));
            }
            TokenKind::Semi | TokenKind::Newline => {
                push_command(&mut pipeline, &mut command);
                if !pipeline.is_empty() {
                    items.push(Item {
                        pipeline: std::mem::take(&mut pipeline),
                        connector: Connector::Always,
                    });
                }
            }
            TokenKind::AndIf | TokenKind::OrIf => {
                push_command(&mut pipeline, &mut command);
                if pipeline.is_empty() {
                    return Err("syntax error near logical operator".to_string());
                }
                items.push(Item {
                    pipeline: std::mem::take(&mut pipeline),
                    connector: if matches!(token.kind, TokenKind::AndIf) {
                        Connector::And
                    } else {
                        Connector::Or
                    },
                });
            }
            TokenKind::Eof => break,
            other => return Err(format!("unsupported syntax: {other:?}")),
        }
    }

    if pending.is_some() {
        return Err("syntax error: redirect without a target".to_string());
    }

    push_command(&mut pipeline, &mut command);
    if !pipeline.is_empty() {
        items.push(Item {
            pipeline,
            connector: Connector::Always,
        });
    }

    Ok(items)
}

fn push_command(pipeline: &mut Vec<SimpleCommand>, command: &mut SimpleCommand) {
    if !command.words.is_empty() || !command.redirects.is_empty() || !command.assignments.is_empty()
    {
        pipeline.push(std::mem::take(command));
    }
}

fn as_assignment(word: &Word) -> Option<(String, Word)> {
    let Part::Literal(text) = word.parts.first()? else {
        return None;
    };
    let (name, rest) = text.split_once('=')?;
    if !is_name(name) {
        return None;
    }

    let mut parts = Vec::new();
    if !rest.is_empty() {
        parts.push(Part::Literal(rest.to_string()));
    }
    parts.extend(word.parts[1..].iter().cloned());
    Some((name.to_string(), Word { parts }))
}

fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}

fn expand_assignment(word: &Word, shell: &Shell) -> String {
    let mut out = String::new();
    expand_text(&word.parts, shell, &mut out);
    out
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
            Part::Variable(name) => fields.push(&shell.var(name), false),
        }
    }

    fields.finish()
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
