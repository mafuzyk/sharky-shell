mod arith;
mod ast;
mod cli;
mod editor;
mod exec;
mod histexpand;
mod history;
mod lexer;
mod parser;
mod profiles;
mod prompt;
mod repl;
mod term;
mod test_expr;
mod z;
mod shell;

fn main() {
    std::process::exit(cli::run());
}
