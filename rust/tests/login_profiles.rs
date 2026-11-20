//! Login startup behaviour: the standard profile files are executed, in bash's
//! order, rather than scanned for literal assignments.
//!
//! Every fixture lives in a throwaway `HOME`; the user's real dotfiles are
//! never read or written.

use std::io::Write;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use nix::pty::openpty;

/// Points the shell at a configuration directory that does not exist so the
/// test observes profile handling alone.
const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sharky-login-profiles-{}-{name}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("HOME", home)
        .output()
        .expect("failed to run sharky")
}

/// Spawns a login shell on a pseudo-terminal so it takes the interactive path.
fn login_pty(home: &Path) -> (Child, std::os::fd::OwnedFd) {
    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-l"])
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("TERM", "xterm-256color")
        .env("HOME", home)
        .spawn()
        .expect("spawn sharky");

    (child, pty.master)
}

/// Waits for `child` to exit, failing the test instead of hanging forever.
fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("wait for sharky") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("sharky did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Appends `name` to `marker` when the file is executed.
fn append_marker(marker: &Path, name: &str) -> String {
    format!("printf '%s\\n' {name} >> '{}'\n", marker.display())
}

fn markers(marker: &Path) -> Vec<String> {
    std::fs::read_to_string(marker)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn login_executes_the_first_user_profile_in_bash_order() {
    let home = scratch("order");
    let marker = home.join("order.log");
    let body = append_marker(&marker, "profile");
    std::fs::write(home.join(".profile"), &body).unwrap();
    std::fs::write(home.join(".bash_login"), append_marker(&marker, "bash_login")).unwrap();
    std::fs::write(
        home.join(".bash_profile"),
        append_marker(&marker, "bash_profile"),
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".config/sharky")).unwrap();
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        append_marker(&marker, "profile.sharky"),
    )
    .unwrap();

    // `.bash_profile` wins, and the sharky hook runs after the bash chain.
    let out = run(&home, &["-l", "-c", "true"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(markers(&marker), ["bash_profile", "profile.sharky"]);

    std::fs::remove_file(home.join(".bash_profile")).unwrap();
    let out = run(&home, &["-l", "-c", "true"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(markers(&marker), ["bash_profile", "profile.sharky", "bash_login", "profile.sharky"]);

    std::fs::remove_file(home.join(".bash_login")).unwrap();
    let out = run(&home, &["-l", "-c", "true"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        markers(&marker),
        [
            "bash_profile",
            "profile.sharky",
            "bash_login",
            "profile.sharky",
            "profile",
            "profile.sharky",
        ]
    );

    // Without the login flag no profile runs at all.
    let before = markers(&marker);
    let out = run(&home, &["-c", "true"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(markers(&marker), before);

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_expands_path_prepends_from_a_profile() {
    let home = scratch("path");
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(
        home.join(".profile"),
        "export PATH=\"$HOME/bin:$PATH\"\nexport PATH=\"${HOME}/bin:${PATH}\"\n",
    )
    .unwrap();

    let out = run(&home, &["-l", "-c", r#"printf '%s' "$PATH""#]);
    assert!(out.status.success(), "{out:?}");
    let path = String::from_utf8_lossy(&out.stdout);
    let bin = format!("{}/bin", home.display());

    // Both spellings take effect, so the directory is prepended twice: the
    // second line repeats it. What matters is that expansions are executed
    // instead of being dropped by a literal scan.
    assert!(path.contains(&bin), "{path}");
    assert_eq!(path.matches(&bin).count(), 2, "{path}");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_does_not_duplicate_path_entries_from_the_profiles_it_executes() {
    let home = scratch("dedupe");
    std::fs::create_dir_all(home.join("profilebin")).unwrap();
    std::fs::create_dir_all(home.join("rcbin")).unwrap();
    std::fs::write(
        home.join(".profile"),
        "export PATH=\"$HOME/profilebin:$PATH\"\n",
    )
    .unwrap();
    // Only other shells read `.bashrc`, so its PATH edits are still offered to
    // this shell by discovery; the executed login profile is not scanned twice.
    std::fs::write(home.join(".bashrc"), "export PATH=\"$HOME/rcbin:$PATH\"\n").unwrap();

    let out = run(&home, &["-l", "-c", r#"printf '%s' "$PATH""#]);
    assert!(out.status.success(), "{out:?}");
    let path = String::from_utf8_lossy(&out.stdout);
    let profilebin = format!("{}/profilebin", home.display());
    let rcbin = format!("{}/rcbin", home.display());
    assert_eq!(path.matches(&profilebin).count(), 1, "{path}");
    assert!(path.contains(&rcbin), "{path}");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_runs_a_file_sourced_by_a_profile() {
    let home = scratch("sourced");
    let marker = home.join("sourced.log");
    std::fs::write(
        home.join("child.sh"),
        format!(
            "CHILD_VAR=from-child\n{}",
            append_marker(&marker, "child")
        ),
    )
    .unwrap();
    std::fs::write(
        home.join(".profile"),
        ". \"$HOME/child.sh\"\nif [ -n \"$CHILD_VAR\" ]; then\nprintf 'branch\\n' >> \"$HOME/sourced.log\"\nfi\n",
    )
    .unwrap();

    let out = run(&home, &["-l", "-c", r#"printf '%s' "$CHILD_VAR""#]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from-child", "{out:?}");
    // Conditionals in the profile are evaluated too.
    assert_eq!(markers(&marker), ["child", "branch"]);

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_reports_a_broken_profile_and_keeps_going() {
    let home = scratch("broken");
    std::fs::write(home.join(".profile"), "BEFORE=yes\n)\nAFTER=no\n").unwrap();
    std::fs::create_dir_all(home.join(".config/sharky")).unwrap();
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        "HOOK=ran\n",
    )
    .unwrap();

    let out = run(
        &home,
        &[
            "-l",
            "-c",
            r#"printf '%s|%s|%s|%s' "$BEFORE" "$AFTER" "$HOOK" "$FOO""#,
        ],
    );
    assert!(out.status.success(), "{out:?}");
    // Statements above the error still run, as bash does; the rest of the login
    // sequence survives the broken file.
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes||ran|");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("{}/.profile:2:", home.display())),
        "{stderr}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_survives_a_home_without_profiles() {
    let home = scratch("empty");
    let out = run(&home, &["-l", "-c", "printf OK"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "OK");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_profile_plain_assignments_do_not_leak_into_a_child_environment() {
    let home = scratch("profile-export");
    std::fs::write(
        home.join(".profile"),
        "LOGGED_PLAIN=one\nexport LOGGED_EXPORTED=two\n",
    )
    .unwrap();

    let out = run(&home, &["-l", "-c", "env"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.lines().any(|line| line.starts_with("LOGGED_PLAIN=")),
        "{text}"
    );
    assert!(
        text.lines().any(|line| line == "LOGGED_EXPORTED=two"),
        "{text}"
    );
    // The system profile marks itself with a plain assignment, which must not
    // become part of the environment inherited by commands either.
    assert!(
        !text.lines().any(|line| line.starts_with("__ETC_PROFILE_SOURCED=")),
        "{text}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn exit_in_a_login_profile_stops_the_c_command() {
    let home = scratch("exit-c");
    std::fs::write(home.join(".profile"), "exit 5\n").unwrap();

    // bash runs the profile and never reaches the `-c` string: exit status 5,
    // no `RAN` on stdout.
    let out = run(&home, &["-l", "-c", "echo RAN"]);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("RAN"),
        "{out:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn exit_zero_in_a_login_profile_exits_cleanly() {
    let home = scratch("exit-zero");
    std::fs::write(home.join(".profile"), "exit 0\n").unwrap();

    let out = run(&home, &["-l", "-c", "echo RAN"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("RAN"),
        "{out:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn exports_before_exit_still_apply() {
    let home = scratch("exit-exports");
    std::fs::write(
        home.join(".profile"),
        "trap 'printf \"%s\" \"$FOO\" > \"$HOME/trap.log\"' EXIT\n\
         export FOO=bar\n\
         exit 7\n",
    )
    .unwrap();

    // bash applies the export, then `exit 7` runs the EXIT trap with the
    // exported value visible and skips the `-c` string.
    let out = run(&home, &["-l", "-c", "echo RAN"]);
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("RAN"),
        "{out:?}"
    );
    let seen = std::fs::read_to_string(home.join("trap.log")).unwrap_or_default();
    assert_eq!(seen, "bar", "{out:?}");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn exit_in_a_login_profile_stops_the_interactive_shell() {
    let home = scratch("exit-pty");
    std::fs::write(home.join(".profile"), "exit 5\n").unwrap();

    let (mut child, master) = login_pty(&home);
    let mut writer =
        unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };
    // If the REPL starts despite the profile's exit, this line leaves a marker.
    let _ = writer.write_all(b"printf TYPED > \"$HOME/typed.log\"\r");

    let status = wait_for_exit(&mut child);
    assert_eq!(status.code(), Some(5), "{status:?}");
    assert!(
        !home.join("typed.log").exists(),
        "the REPL executed a line after the profile requested exit"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn json_login_honors_a_profile_exit() {
    let home = scratch("exit-json");
    std::fs::write(home.join(".profile"), "exit 5\n").unwrap();

    let out = run(&home, &["--json", "-l", "-c", "printf RAN"]);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"status\":5"), "{text}");
    assert!(!text.contains("RAN"), "{text}");

    std::fs::remove_dir_all(&home).ok();
}
