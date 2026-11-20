//! Login startup behaviour: the standard profile files are executed, in bash's
//! order, rather than scanned for literal assignments.
//!
//! Every fixture lives in a throwaway `HOME`; the user's real dotfiles are
//! never read or written.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
