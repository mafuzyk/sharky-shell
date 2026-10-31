mod arith;
mod ast;
mod cli;
mod editor;
mod config;
mod exec;
mod history;
mod lexer;
mod parser;
mod prompt;
mod repl;
mod term;
mod test_expr;
mod shell;

fn main() {
    std::process::exit(cli::run());
}
