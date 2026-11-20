use crate::editor::{Editor, Line};
use crate::histexpand;
use crate::prompt;
use crate::shell::Shell;
use crate::term;

pub fn run(shell: &mut Shell, private: bool) -> i32 {
    install_signals();
    shell.set_interactive(true);

    shell.prepare_history(private);
    let mut editor = Editor::new();
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

        // A trap delivered while the shell sat at the prompt runs here, before
        // the next line is read, while the terminal is still in cooked mode.
        if let Some(code) = shell.service_traps() {
            return shell.finish(code);
        }

        let primary = buffer.is_empty();
        if primary {
            // Another session's commands become visible here, before the prompt is
            // drawn, so the editor list and the ghost include them. One stat; the
            // file is only read when it actually changed, and never on a
            // continuation line.
            shell.refresh_history();
        }
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
                    match histexpand::expand(&text, &shell.history_expansion_entries()) {
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
                // A Ctrl-C handled while the previous line was being edited (or
                // between commands) must not abort the command about to start.
                crate::signal::clear_interrupt();
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
                // The editor consumed Ctrl-C as a byte; an installed INT action
                // still has to run, and may ask the shell to exit.
                crate::signal::clear_interrupt();
                let exit = shell.interrupt_line();
                buffer.clear();
                shell.last_status = 130;
                status = 130;
                if let Some(code) = exit {
                    return shell.finish(code);
                }
            }
            Ok(Line::Eof) => return shell.finish(status),
            Err(err) => {
                eprintln!("sharky: {err}");
                return 1;
            }
        }
    }
}

fn install_signals() {
    use nix::sys::signal::{SigHandler, Signal, signal};

    // SAFETY: an interactive shell must survive the signals the terminal sends
    // to its own process group. SIGINT is recorded instead of ignored so a
    // builtin loop or a blocked `read` can still be interrupted; a foreground
    // job runs in its own process group and receives its own copy.
    unsafe {
        crate::signal::interruptible(Signal::SIGINT as i32);
        let _ = signal(Signal::SIGQUIT, SigHandler::SigIgn);
        let _ = signal(Signal::SIGTSTP, SigHandler::SigIgn);
        let _ = signal(Signal::SIGTTOU, SigHandler::SigIgn);
        let _ = signal(Signal::SIGTTIN, SigHandler::SigIgn);
        let _ =
            signal(Signal::SIGTERM, SigHandler::Handler(crate::term::restore_terminal_and_exit));
        let _ =
            signal(Signal::SIGHUP, SigHandler::Handler(crate::term::restore_terminal_and_exit));
    }
}
