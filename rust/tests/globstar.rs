use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn sharky_in(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .current_dir(dir)
        .output()
        .expect("failed to run sharky")
}

fn temp_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sharky-globstar-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture() -> std::path::PathBuf {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("mid/deep")).unwrap();
    std::fs::write(dir.join("a.txt"), "").unwrap();
    std::fs::write(dir.join("mid/b.txt"), "").unwrap();
    std::fs::write(dir.join("mid/deep/c.txt"), "").unwrap();
    dir
}

fn sorted_lines(out: &Output) -> Vec<String> {
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

#[test]
fn globstar_matches_zero_and_multiple_directory_levels() {
    let dir = fixture();
    let out = sharky_in(
        &dir,
        &["-c", r#"shopt -s globstar; printf '%s\n' **/*.txt"#],
    );
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec!["a.txt", "mid/b.txt", "mid/deep/c.txt"]
    );
}

#[test]
fn shopt_u_globstar_restores_single_level_matching() {
    let dir = fixture();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            r#"shopt -s globstar; shopt -u globstar; printf '%s\n' **/*.txt"#,
        ],
    );
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(sorted_lines(&out), vec!["mid/b.txt"]);
}
