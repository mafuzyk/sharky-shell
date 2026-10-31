mod arith;
mod ast;
mod cli;
mod editor;
mod config;
mod exec;
mod lexer;
mod parser;
mod prompt;
mod repl;
mod term;
mod shell;

fn main() {
    std::process::exit(cli::run());
}
