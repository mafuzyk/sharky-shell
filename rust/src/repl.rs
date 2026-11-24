use crate::editor::{Editor, Line};
use crate::histexpand;
use crate::prompt;
use crate::shell::Shell;
use crate::term;

pub fn run(shell: &mut Shell, private: bool) -> i32 {
    install_signals();
    shell.set_interactive(true);
    maybe_prompt_config_migration(shell);

    shell.prepare_history(private);
    let mut editor = Editor::new();
    let mut buffer = String::new();
    let mut status = 0;
    let mut pending_end: Option<i32> = None;
    let mut consecutive_eofs: usize = 0;

    if shell.setting("greeting") == Some("on") {
        println!("sharky {}", env!("CARGO_PKG_VERSION"));
    }
    for command in shell.wake_commands().to_vec() {
        let _ = shell.eval(&command);
    }
    shell.last_status = 0;

    loop {
        shell.reap_jobs();
        // The boundary reaped whatever the prompt did not; the flag it raised
        // must not wake the editor for a job that is no longer there.
        crate::signal::take_child_pending();
        crate::signal::take_winch_pending();
        shell.reap_substitutions_idle();

        // A trap delivered while the shell sat at the prompt runs here, before
        // the next line is read, while the terminal is still in cooked mode.
        if let Some(code) = shell.service_traps() {
            return shell.finish(code);
        }
        // An untrapped fatal signal recorded by the handler ends the shell
        // here, with the same status a killed command would report.
        if let Some(signal) = crate::signal::fatal_signal() {
            return shell.finish(128 + signal);
        }

        let primary = buffer.is_empty();
        if primary {
            // Another session's commands become visible here, before the prompt is
            // drawn, so the editor list and the ghost include them. One stat; the
            // file is only read when it actually changed, and never on a
            // continuation line.
            shell.refresh_history();
            shell.run_hook("precmd", &[]);
        }
        let (raw_prompt, rprompt) = if primary {
            (prompt::render_raw(shell), prompt::render_right(shell))
        } else {
            (prompt::render_continuation(shell), None)
        };
        let input_marker = if primary {
            if let Some(code) = pending_end.take() {
                let _ = term::write_stdout(&term::mark_command_end(code));
            }
            let _ = term::write_stdout(&term::mark_working_directory(&shell.cwd));
            let _ = term::write_stdout(term::MARK_PROMPT_START);
            term::MARK_INPUT_START
        } else {
            ""
        };

        match editor.read_line(&raw_prompt, input_marker, rprompt.as_deref(), shell) {
            Ok(Line::Content(mut text)) => {
                consecutive_eofs = 0;
                if buffer.is_empty() && text.trim().is_empty() {
                    status = shell.last_status;
                    continue;
                }
                let first_word = text.split_whitespace().next().unwrap_or("");
                if !matches!(first_word, "exit" | "logout") {
                    shell.reset_stopped_jobs_warning();
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
                shell.run_hook("preexec", std::slice::from_ref(&buffer));
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
                consecutive_eofs = 0;
                shell.reset_stopped_jobs_warning();
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
            Ok(Line::Eof) => {
                // A terminal hangup reaches the editor as end of input; the
                // fatal handler recorded the signal, and the EXIT action still
                // has to run before the shell leaves. A hangup whose SIGHUP is
                // still in flight is recognized by the terminal itself.
                let fatal = crate::signal::fatal_signal();
                let lost = term::terminal_lost();
                if fatal.is_none() && !lost {
                    let eof_var = shell.var("IGNOREEOF");
                    if shell.ignoreeof_enabled() || !eof_var.is_empty() {
                        let threshold = eof_var.trim().parse::<usize>().unwrap_or(10);
                        if consecutive_eofs < threshold {
                            consecutive_eofs += 1;
                            let cmd = if shell.is_login_shell() {
                                "logout"
                            } else {
                                "exit"
                            };
                            eprintln!("Use \"{cmd}\" to leave the shell.");
                            continue;
                        }
                    }
                    if shell.check_stopped_jobs_before_exit() {
                        continue;
                    }
                }
                let code = match fatal {
                    Some(signal) => 128 + signal,
                    None if lost => 129,
                    None => status,
                };
                return shell.finish(code);
            }
            Err(err) => {
                if let Some(signal) = crate::signal::fatal_signal() {
                    return shell.finish(128 + signal);
                }
                if term::terminal_lost() {
                    return shell.finish(129);
                }
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
        crate::signal::fatal(Signal::SIGTERM as i32);
        crate::signal::fatal(Signal::SIGHUP as i32);
        // Finished background jobs are announced while the prompt waits, so
        // the handler both interrupts the input read and records the change.
        crate::signal::watch_children();
        crate::signal::watch_winch();
    }
}

fn maybe_prompt_config_migration(shell: &mut Shell) {
    use std::io::Write;

    let files = shell.legacy_config_files().to_vec();
    if files.is_empty() || std::env::var_os("SHARKY_NO_MIGRATE_PROMPT").is_some() {
        return;
    }

    eprintln!("sharky: deprecated config directives detected in:");
    for path in &files {
        eprintln!("  {}", path.display());
    }
    eprintln!(
        "Legacy keywords (tide, pearl, current, reef, shoal, anchor, coral, wave, wake) will be removed in a future release."
    );
    eprint!("Migrate automatically to the new syntax now? (backups saved as *.bak) [y/N] ");
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        shell.clear_legacy_config_files();
        return;
    }
    if matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "s" | "sim"
    ) {
        let mut total = 0usize;
        for path in &files {
            match sharky_config::migrate_file(path) {
                Ok(n) => total += n,
                Err(err) => eprintln!("sharky: migrate {}: {err}", path.display()),
            }
        }
        shell.clear_legacy_config_files();
        let _ = shell.reload_config();
        eprintln!("sharky: migrated {total} directive(s).");
    } else {
        shell.clear_legacy_config_files();
    }
}
