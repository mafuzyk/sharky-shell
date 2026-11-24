mod agent;
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
mod test_posix;
mod z;
mod shell;

fn main() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        term::restore_on_panic();
        default_hook(info);
    }));
    std::process::exit(cli::run());
}
