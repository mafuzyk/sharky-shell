mod arith;
mod ast;
mod cli;
mod config;
mod exec;
mod lexer;
mod parser;
mod shell;

fn main() {
    std::process::exit(cli::run());
}
