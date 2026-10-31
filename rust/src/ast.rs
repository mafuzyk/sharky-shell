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
    While {
        condition: List,
        body: List,
        until: bool,
    },
    Case {
        word: Word,
        arms: Vec<(Vec<Word>, List)>,
    },
    Function {
        name: String,
        body: List,
    },
    Test(Vec<Word>),
    Arithmetic(String),
    Redirected {
        command: Box<Command>,
        redirects: Vec<Redirect>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Simple {
    pub assignments: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone)]
pub struct Assignment {
    pub name: String,
    pub index: Option<String>,
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
    Append,
    In,
    Dup(i32),
    Here { body: String, expand: bool },
    HereString,
    Both { append: bool },
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
