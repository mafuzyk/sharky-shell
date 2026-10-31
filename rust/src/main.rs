mod cli;
mod exec;
mod lexer;
mod shell;

fn main() {
    std::process::exit(cli::run());
}
