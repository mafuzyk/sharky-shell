use crate::shell::Shell;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run() -> i32 {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut no_config = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => {
                println!("sharky {VERSION}");
                return 0;
            }
            "--help" | "-h" => {
                print_help();
                return 0;
            }
            "--no-config" => no_config = true,
            "-c" => match args.next() {
                Some(source) => command = Some(source),
                None => {
                    eprintln!("sharky: -c requires a command");
                    return 2;
                }
            },
            other => {
                eprintln!("sharky: unsupported argument: {other}");
                return 2;
            }
        }
    }

    let mut shell = Shell::new();
    shell.load_config(no_config);

    match command {
        Some(source) => shell.run(&source),
        None => {
            eprintln!("sharky: interactive mode is not implemented yet");
            2
        }
    }
}

fn print_help() {
    println!("Usage: sharky [OPTIONS]");
    println!("       sharky -c COMMAND");
    println!();
    println!("  -c COMMAND   run COMMAND and exit");
    println!("  --no-config  do not load the configuration file");
    println!("  --version    print the version");
    println!("  --help       print this help");
}
