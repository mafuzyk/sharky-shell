use crate::editor::{Editor, Line};
use crate::histexpand;
use crate::prompt;
use crate::shell::Shell;
use crate::term;

pub fn run(shell: &mut Shell) -> i32 {
    install_signals();
    shell.set_interactive(true);

    let mut editor = Editor::new();
    shell.init_history();
    let mut buffer = String::new();
    let mut status = 0;
    let mut pending_end: Option<i32> = None;

    if shell.setting("greeting") == Some("on") {
        println!("sharky {}", env!("CARGO_PKG_VERSION"));
    }
    for command in shell.wake_commands().to_vec() {
        let _ = shell.eval(&command);
    }
    shell.last_status = 0;

    loop {
        shell.reap_jobs();
        shell.reap_substitutions_idle();

        let primary = buffer.is_empty();
        let mut prompt = if primary {
            prompt::render(shell)
        } else {
            "... ".to_string()
        };
        if primary {
            if let Some(code) = pending_end.take() {
                let _ = term::write_stdout(&term::mark_command_end(code));
            }
            let _ = term::write_stdout(&term::mark_working_directory(&shell.cwd));
            let _ = term::write_stdout(term::MARK_PROMPT_START);
            prompt.push_str(term::MARK_INPUT_START);
        }

        match editor.read_line(&prompt, shell) {
            Ok(Line::Content(mut text)) => {
                if buffer.is_empty() && text.trim().is_empty() {
                    status = shell.last_status;
                    continue;
                }

                if buffer.is_empty() && shell.histexpand() {
                    match histexpand::expand(&text, shell.history_entries()) {
                        Ok(expanded) => {
                            if expanded != text {
                                println!("{expanded}");
                                text = expanded;
                            }
                        }
                        Err(message) => {
                            eprintln!("sharky: {message}");
                            shell.last_status = 1;
                            status = 1;
                            pending_end = Some(1);
                            continue;
                        }
                    }
                }

                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&text);

                let started = std::time::Instant::now();
                let outcome = shell.eval_with(&buffer, || {
                    let _ = term::write_stdout(&term::mark_command_start(&buffer));
                });
                shell.last_duration_ms = started.elapsed().as_millis() as u64;
                shell.close_substitutions_from(0);

                match outcome {
                    Ok(code) => {
                        status = code;
                        pending_end = Some(code);
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
                        pending_end = Some(2);
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
