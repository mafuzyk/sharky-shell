//! Regression tests for non-UTF-8 input honesty (P1-11).
//!
//! Sharky still converts command-line arguments, environment values and the
//! working directory lossily into `String`. That conversion must never be
//! silent: one explicit warning naming the affected item kind must reach
//! stderr, and the warning must not echo the invalid bytes.

use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn sharky() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_sharky")).to_path_buf()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn invalid_utf8_argument_warns_once_without_echoing_bytes() {
    let dir = std::env::temp_dir().join(format!(
        "sharky-nonutf8-arg-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create script directory");
    let script = dir.join("args.sh");
    std::fs::write(&script, b"printf ok\n").expect("write script");
    let invalid = std::ffi::OsString::from_vec(vec![b'a', 0xff, b'b']);
    let output = Command::new(sharky())
        .arg("--no-config")
        .arg(&script)
        .arg(invalid)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/tmp")
        .stdin(Stdio::null())
        .output()
        .expect("run sharky with invalid UTF-8 argument");
    std::fs::remove_dir_all(&dir).ok();

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("non-UTF-8 command-line argument"),
        "expected argument warning, got {stderr:?}"
    );
    assert_eq!(
        stderr.matches("non-UTF-8").count(),
        1,
        "expected exactly one warning, got {stderr:?}"
    );
    assert!(
        !stderr.contains('\u{fffd}'),
        "warning must not include replacement characters: {stderr:?}"
    );
}

#[test]
fn invalid_utf8_environment_value_warns_once_without_echoing_bytes() {
    let invalid = std::ffi::OsString::from_vec(vec![0xff, b'x']);
    let output = Command::new(sharky())
        .arg("--no-config")
        .arg("-c")
        .arg("printf ok")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/tmp")
        .env("SHARKY_BAD_VALUE", invalid)
        .stdin(Stdio::null())
        .output()
        .expect("run sharky with invalid UTF-8 environment value");

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("non-UTF-8 environment variable"),
        "expected environment warning, got {stderr:?}"
    );
    assert_eq!(
        stderr.matches("non-UTF-8").count(),
        1,
        "expected exactly one warning, got {stderr:?}"
    );
    assert!(
        !stderr.contains('\u{fffd}'),
        "warning must not include replacement characters: {stderr:?}"
    );
}

#[test]
fn invalid_utf8_environment_value_warns_on_agent_stderr() {
    let invalid = std::ffi::OsString::from_vec(vec![0xff, b'x']);
    let mut child = Command::new(sharky())
        .args(["agent", "--stdio", "--no-config"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/tmp")
        .env("SHARKY_BAD_VALUE", invalid)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sharky agent --stdio");
    let stdin = child.stdin.take().expect("agent stdin");
    let mut stdout = child.stdout.take().expect("agent stdout");

    let mut ready = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.ends_with(b"\n") {
        let mut byte = [0u8; 1];
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the agent ready frame"
        );
        assert_eq!(
            stdout.read(&mut byte).expect("read agent ready frame"),
            1,
            "agent closed before the ready frame"
        );
        ready.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&ready).contains("\"type\":\"ready\""),
        "unexpected ready frame: {ready:?}"
    );
    drop(stdin);
    let status = child.wait().expect("wait for the agent");
    let mut stderr = String::new();
    child
        .stderr
        .as_mut()
        .expect("agent stderr")
        .read_to_string(&mut stderr)
        .expect("read agent stderr");

    assert!(status.success(), "{status:?}: stderr={stderr:?}");
    assert!(
        stderr.contains("non-UTF-8 environment variable"),
        "expected env warning on the real stderr of the agent, got {stderr:?}"
    );
    assert!(
        !stderr.contains('\u{fffd}'),
        "warning must not include replacement characters: {stderr:?}"
    );
}

#[test]
fn invalid_utf8_cwd_warns_once_without_echoing_bytes() {
    let base = std::env::temp_dir().join(format!(
        "sharky-nonutf8-cwd-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let mut raw = base.clone().into_os_string().into_vec();
    raw.push(b'/');
    raw.push(b'x');
    raw.push(0xff);
    let invalid = PathBuf::from(std::ffi::OsString::from_vec(raw));
    std::fs::create_dir_all(&invalid).expect("create invalid UTF-8 directory");

    let output = Command::new(sharky())
        .arg("--no-config")
        .arg("-c")
        .arg("printf ok")
        .current_dir(&invalid)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/tmp")
        .stdin(Stdio::null())
        .output()
        .expect("run sharky from invalid UTF-8 cwd");

    std::fs::remove_dir_all(&invalid).ok();
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("non-UTF-8 working directory"),
        "expected cwd warning, got {stderr:?}"
    );
    assert_eq!(
        stderr.matches("non-UTF-8").count(),
        1,
        "expected exactly one warning, got {stderr:?}"
    );
    assert!(
        !stderr.contains('\u{fffd}'),
        "warning must not include replacement characters: {stderr:?}"
    );
}
