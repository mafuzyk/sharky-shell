use crate::lexer::{Part, Word};

#[derive(Debug, Clone)]
pub struct Program {
    pub lines: Vec<List>,
}

#[derive(Debug, Clone, Default)]
pub struct List {
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub line: usize,
    pub background: bool,
    pub and_or: AndOr,
}

#[derive(Debug, Clone)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(bool, Pipeline)>,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub commands: Vec<Command>,
    /// For each pipe link, whether it was written as `|&` (stderr of the left
    /// stage joins the pipe alongside stdout).
    pub merge_stderr: Vec<bool>,
    pub timed: Option<bool>,
    pub negate: bool,
}

#[derive(Debug, Clone)]
pub enum Command {
    Simple(Simple),
    Not(Box<Command>),
    Subshell(List),
    Group(List),
    If {
        branches: Vec<(List, List)>,
        else_body: Option<List>,
    },
    For {
        name: String,
        words: Vec<Word>,
        body: List,
    },
    ForArith {
        init: String,
        condition: String,
        step: String,
        body: List,
    },
    While {
        condition: List,
        body: List,
        until: bool,
    },
    Case {
        word: Word,
        arms: Vec<CaseArm>,
    },
    Function {
        name: String,
        body: List,
        line: usize,
    },
    Test(Vec<Word>),
    Arithmetic(String),
    Redirected {
        command: Box<Command>,
        redirects: Vec<Redirect>,
    },
    Coproc {
        name: String,
        command: Box<Command>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Simple {
    pub line: usize,
    pub assignments: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone)]
pub struct Assignment {
    pub name: String,
    pub index: Option<Word>,
    pub append: bool,
    pub value: AssignmentValue,
}

#[derive(Debug, Clone)]
pub enum AssignmentValue {
    Word(Word),
    Array(Vec<Word>),
}

#[derive(Debug, Clone)]
pub struct Redirect {
    pub fd: i32,
    pub op: RedirectOp,
    pub target: Option<Word>,
}

#[derive(Debug, Clone)]
pub enum RedirectOp {
    Out,
    OutForce,
    Append,
    In,
    Dup(i32),
    DupIn(i32),
    DupWord,
    DupInWord,
    Close,
    Here { body: String, expand: bool },
    HereString,
    Both { append: bool },
}

pub type CaseArm = (Vec<Word>, List, CaseTerm);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaseTerm {
    Break,
    Fallthrough,
    Continue,
}

pub fn literal(word: &Word) -> Option<String> {
    let mut text = String::new();
    for part in &word.parts {
        match part {
            Part::Literal(value) | Part::Single(value) | Part::Escaped(value) => text.push_str(value),
            _ => return None,
        }
    }
    Some(text)
}
