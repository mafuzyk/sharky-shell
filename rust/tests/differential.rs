use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ENV: &[(&str, &str)] = &[
    ("PATH", "/usr/bin:/bin"),
    ("HOME", "/tmp"),
    ("LC_ALL", "C.UTF-8"),
    ("DIFF_X", "a  b"),
];

fn run(bin: &Path, source: &str) -> Output {
    Command::new(bin)
        .args(["--no-config", "-c", source])
        .env_clear()
        .envs(ENV.iter().copied())
        .output()
        .unwrap_or_else(|err| panic!("failed to run {}: {err}", bin.display()))
}

fn reference() -> Option<PathBuf> {
    let path = std::env::var("SHARKY_REF").map(PathBuf::from).unwrap_or_else(|_| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../bin/sharky")
    });
    path.exists().then_some(path)
}

#[test]
fn matches_the_ruby_reference() {
    let Some(reference) = reference() else {
        eprintln!("skipping differential test: no reference binary (set SHARKY_REF)");
        return;
    };

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/cases");
    let mut cases: Vec<_> = fs::read_dir(&dir)
        .expect("conformance/cases must exist")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sh"))
        .collect();
    cases.sort();

    let binary = Path::new(env!("CARGO_BIN_EXE_sharky"));
    let mut failures = Vec::new();

    for case in &cases {
        let source = fs::read_to_string(case).expect("case must be readable");
        let source = source.strip_suffix('\n').unwrap_or(&source);
        let name = case.file_stem().unwrap().to_string_lossy().into_owned();

        let expected = run(&reference, source);
        let actual = run(binary, source);

        let mut diffs = Vec::new();
        if expected.stdout != actual.stdout {
            diffs.push(format!(
                "  stdout\n    ruby: {:?}\n    rust: {:?}",
                String::from_utf8_lossy(&expected.stdout),
                String::from_utf8_lossy(&actual.stdout)
            ));
        }
        if expected.stderr != actual.stderr {
            diffs.push(format!(
                "  stderr\n    ruby: {:?}\n    rust: {:?}",
                String::from_utf8_lossy(&expected.stderr),
                String::from_utf8_lossy(&actual.stderr)
            ));
        }
        if expected.status.code() != actual.status.code() {
            diffs.push(format!(
                "  status\n    ruby: {:?}\n    rust: {:?}",
                expected.status.code(),
                actual.status.code()
            ));
        }
        if !diffs.is_empty() {
            failures.push(format!("{name}\n{}", diffs.join("\n")));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n\n")
    );
}
