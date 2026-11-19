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

#[test]
fn globstar_is_off_by_default() {
    let dir = fixture();
    let out = sharky_in(&dir, &["-c", r#"printf '%s\n' **/*.txt"#]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(sorted_lines(&out), vec!["mid/b.txt"]);
}

#[test]
fn quoted_globstar_stays_literal() {
    let dir = fixture();
    std::fs::write(dir.join("a**"), "").unwrap();
    let cases = [
        (r#"shopt -s globstar; printf '<%s>' '**'"#, "<**>"),
        (r#"shopt -s globstar; printf '<%s>' a"**""#, "<a**>"),
    ];
    for (script, expected) in cases {
        let out = sharky_in(&dir, &["-c", script]);
        assert_eq!(out.status.code(), Some(0), "{script}: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn globstar_skips_hidden_entries() {
    let dir = fixture();
    std::fs::create_dir_all(dir.join(".hidden")).unwrap();
    std::fs::write(dir.join(".hidden/d.txt"), "").unwrap();
    std::fs::write(dir.join(".secret.txt"), "").unwrap();
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
fn globstar_allows_an_explicit_hidden_component() {
    let dir = fixture();
    std::fs::create_dir_all(dir.join(".hidden")).unwrap();
    std::fs::write(dir.join(".hidden/d.txt"), "").unwrap();
    let out = sharky_in(
        &dir,
        &["-c", r#"shopt -s globstar; printf '%s\n' **/.hidden/*.txt"#],
    );
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(sorted_lines(&out), vec![".hidden/d.txt"]);
}

#[test]
fn globstar_deduplicates_overlapping_stars() {
    let dir = fixture();
    let out = sharky_in(
        &dir,
        &["-c", r#"shopt -s globstar; printf '%s\n' **/**/*.txt"#],
    );
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec!["a.txt", "mid/b.txt", "mid/deep/c.txt"]
    );
}

#[test]
fn globstar_preserves_trailing_slash() {
    let dir = fixture();
    let out = sharky_in(&dir, &["-c", r#"shopt -s globstar; printf '<%s>' **/"#]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<mid/><mid/deep/>");
}

#[test]
fn globstar_does_not_descend_into_symlinked_dirs() {
    let dir = fixture();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::write(dir.join("target/linked.txt"), "").unwrap();
    std::os::unix::fs::symlink("target", dir.join("linkdir")).unwrap();
    let out = sharky_in(
        &dir,
        &["-c", r#"shopt -s globstar; printf '%s\n' **/*.txt"#],
    );
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec![
            "a.txt",
            "mid/b.txt",
            "mid/deep/c.txt",
            "target/linked.txt"
        ]
    );
}

fn nested_fixture() -> std::path::PathBuf {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("topdir/sub")).unwrap();
    std::fs::create_dir_all(dir.join("realdir")).unwrap();
    std::fs::create_dir_all(dir.join("work")).unwrap();
    std::fs::write(dir.join("a.txt"), "").unwrap();
    std::fs::write(dir.join("topdir/b.txt"), "").unwrap();
    std::fs::write(dir.join("topdir/sub/c.txt"), "").unwrap();
    std::fs::write(dir.join("realdir/d.txt"), "").unwrap();
    std::os::unix::fs::symlink("a.txt", dir.join("link.txt")).unwrap();
    std::os::unix::fs::symlink("topdir", dir.join("linkdir")).unwrap();
    std::os::unix::fs::symlink("a.txt", dir.join("literal")).unwrap();
    std::os::unix::fs::symlink("../realdir", dir.join("topdir/sym")).unwrap();
    dir
}

// Bash 5.3.20 reference observations encoded below:
// - a leading ** never descends symlinked dirs, but a final component still matches symlink entries;
// - ** after a prefix (topdir/, ./, ../) does descend symlinked dirs;
// - terminal ** includes the base dir, while **/** drops its trailing slash unless followed by /.
#[test]
fn dot_prefixed_globstar_keeps_prefix_and_follows_symlink_dirs() {
    let dir = nested_fixture();
    let out = sharky_in(
        &dir,
        &["-c", r#"shopt -s globstar; printf '%s\n' ./**/*.txt"#],
    );
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec![
            "./a.txt",
            "./link.txt",
            "./linkdir/b.txt",
            "./realdir/d.txt",
            "./topdir/b.txt",
            "./topdir/sub/c.txt",
            "./topdir/sym/d.txt",
        ]
    );
}

#[test]
fn parent_prefixed_globstar_keeps_prefix_and_follows_symlink_dirs() {
    let dir = nested_fixture();
    let out = sharky_in(
        &dir.join("work"),
        &["-c", r#"shopt -s globstar; printf '%s\n' ../**/*.txt"#],
    );
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec![
            "../a.txt",
            "../link.txt",
            "../linkdir/b.txt",
            "../realdir/d.txt",
            "../topdir/b.txt",
            "../topdir/sub/c.txt",
            "../topdir/sym/d.txt",
        ]
    );
}

#[test]
fn terminal_globstar_includes_the_base_directory() {
    let dir = nested_fixture();
    let out = sharky_in(&dir, &["-c", r#"shopt -s globstar; printf '%s\n' topdir/**"#]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec![
            "topdir/",
            "topdir/b.txt",
            "topdir/sub",
            "topdir/sub/c.txt",
            "topdir/sym",
        ]
    );

    let out = sharky_in(&dir, &["-c", r#"shopt -s globstar; printf '%s\n' topdir/**/"#]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec!["topdir/", "topdir/sub/", "topdir/sym/"]
    );
}

#[test]
fn leading_globstar_slash_lists_symlink_dirs_without_descending() {
    let dir = nested_fixture();
    let out = sharky_in(&dir, &["-c", r#"shopt -s globstar; printf '%s\n' **/"#]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec![
            "linkdir/",
            "realdir/",
            "topdir/",
            "topdir/sub/",
            "topdir/sym/",
            "work/",
        ]
    );
}

#[test]
fn symlink_entries_match_but_leading_globstar_does_not_descend_them() {
    let dir = nested_fixture();
    let out = sharky_in(&dir, &["-c", r#"shopt -s globstar; printf '%s\n' **/literal"#]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(sorted_lines(&out), vec!["literal"]);

    let out = sharky_in(&dir, &["-c", r#"shopt -s globstar; printf '%s\n' **/*.txt"#]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec![
            "a.txt",
            "link.txt",
            "realdir/d.txt",
            "topdir/b.txt",
            "topdir/sub/c.txt",
        ]
    );
}

#[test]
fn consecutive_globstars_match_the_single_star_set() {
    let dir = nested_fixture();
    let cases = [
        (
            r#"shopt -s globstar; printf '%s\n' **/**/*.txt"#,
            vec![
                "a.txt",
                "link.txt",
                "realdir/d.txt",
                "topdir/b.txt",
                "topdir/sub/c.txt",
            ],
        ),
        (
            r#"shopt -s globstar; printf '%s\n' **/**/"#,
            vec![
                "linkdir/",
                "realdir/",
                "topdir/",
                "topdir/sub/",
                "topdir/sym/",
                "work/",
            ],
        ),
        (
            r#"shopt -s globstar; printf '%s\n' **/**"#,
            vec![
                "a.txt",
                "link.txt",
                "linkdir",
                "literal",
                "realdir",
                "realdir/d.txt",
                "topdir",
                "topdir/b.txt",
                "topdir/sub",
                "topdir/sub/c.txt",
                "topdir/sym",
                "work",
            ],
        ),
    ];
    for (script, expected) in cases {
        let out = sharky_in(&dir, &["-c", script]);
        assert_eq!(out.status.code(), Some(0), "{script}: {out:?}");
        assert_eq!(sorted_lines(&out), expected, "{script}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn terminal_globstar_base_slash_requires_a_literal_prefix() {
    let dir = nested_fixture();
    let cases = [
        (
            r#"shopt -s globstar; printf '%s\n' topdir/sub/**"#,
            vec!["topdir/sub/", "topdir/sub/c.txt"],
        ),
        (
            r#"shopt -s globstar; printf '%s\n' **/sub/**"#,
            vec!["topdir/sub", "topdir/sub/c.txt"],
        ),
    ];
    for (script, expected) in cases {
        let out = sharky_in(&dir, &["-c", script]);
        assert_eq!(out.status.code(), Some(0), "{script}: {out:?}");
        assert_eq!(sorted_lines(&out), expected, "{script}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn consecutive_terminal_globstar_drops_the_base_slash() {
    let dir = nested_fixture();
    let out = sharky_in(
        &dir,
        &["-c", r#"shopt -s globstar; printf '%s\n' topdir/**/**"#],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec![
            "topdir",
            "topdir/b.txt",
            "topdir/sub",
            "topdir/sub/c.txt",
            "topdir/sym",
        ]
    );

    let out = sharky_in(
        &dir,
        &["-c", r#"shopt -s globstar; printf '%s\n' topdir/**/**/"#],
    );
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        sorted_lines(&out),
        vec!["topdir/", "topdir/sub/", "topdir/sym/"]
    );
}
