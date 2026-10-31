use crate::editor::{Editor, Line};
use crate::prompt;
use crate::shell::Shell;

pub fn run(shell: &mut Shell) -> i32 {
    install_signals();
    shell.set_interactive(true);

    let mut editor = Editor::new();
    shell.init_history();
    let mut buffer = String::new();
    let mut status = 0;

    if shell.setting("greeting") == Some("on") {
        println!("sharky {}", env!("CARGO_PKG_VERSION"));
    }

    loop {
        shell.reap_jobs();

        let prompt = if buffer.is_empty() {
            prompt::render(shell)
        } else {
            "... ".to_string()
        };

        match editor.read_line(&prompt, shell) {
            Ok(Line::Content(text)) => {
                if buffer.is_empty() && text.trim().is_empty() {
                    status = shell.last_status;
                    continue;
                }

                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&text);

                let started = std::time::Instant::now();
                let outcome = shell.eval(&buffer);
                shell.last_duration_ms = started.elapsed().as_millis() as u64;

                match outcome {
                    Ok(code) => {
                        status = code;
                        shell.push_history(&buffer);
                        buffer.clear();
                        if let Some(exit) = shell.exit_requested {
                            shell.save_history();
                            return shell.finish(exit);
                        }
                    }
                    Err(err) if err.incomplete => {}
                    Err(err) => {
                        eprintln!("sharky: {}", err.message);
                        shell.push_history(&buffer);
                        buffer.clear();
                        status = 2;
                    }
                }
            }
            Ok(Line::Interrupted) => {
                buffer.clear();
                shell.last_status = 130;
                status = 130;
            }
            Ok(Line::Eof) => {
                shell.save_history();
                return shell.finish(status);
            }
            Err(err) => {
                eprintln!("sharky: {err}");
                shell.save_history();
                return 1;
            }
        }
    }
}

fn install_signals() {
    use nix::sys::signal::{SigHandler, Signal, signal};

    // SAFETY: ignoring these signals is how an interactive shell survives them;
    // foreground jobs run in their own process group and receive their own copy.
    unsafe {
        let _ = signal(Signal::SIGINT, SigHandler::SigIgn);
        let _ = signal(Signal::SIGQUIT, SigHandler::SigIgn);
        let _ = signal(Signal::SIGTSTP, SigHandler::SigIgn);
        let _ = signal(Signal::SIGTTOU, SigHandler::SigIgn);
        let _ = signal(Signal::SIGTTIN, SigHandler::SigIgn);
    }
}
