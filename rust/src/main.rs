mod arith;
mod ast;
mod cli;
mod diagnostics;
mod editor;
mod exec;
mod help;
mod highlight;
mod histexpand;
mod history;
mod lexer;
mod parser;
mod profiles;
mod prompt;
mod repl;
mod signal;
mod term;
mod test_expr;
mod z;
mod shell;

fn main() {
    std::process::exit(cli::run());
}
